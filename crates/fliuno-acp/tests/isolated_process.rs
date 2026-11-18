#![cfg(all(windows, feature = "test-fixtures"))]

use fliuno_acp::{CancellationToken, Client, ClientOptions, Error, Event, schema::*};
use std::{collections::BTreeMap, path::PathBuf, process::Stdio, time::Duration};

async fn update(events: &mut fliuno_acp::Events) -> String {
    let Some(Event::Update(notification)) =
        tokio::time::timeout(Duration::from_secs(5), events.recv())
            .await
            .unwrap()
    else {
        panic!("expected update")
    };
    let SessionUpdate::AgentMessageChunk(chunk) = notification.update else {
        panic!("expected message")
    };
    let ContentBlock::Text(text) = chunk.content else {
        panic!("expected text")
    };
    text.text
}

fn executable() -> PathBuf {
    env!("CARGO_BIN_EXE_fliuno-acp-fixture").into()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn actual_acp_stdio_isolates_agent_tools_and_stops_process_on_timeout() {
    let root = std::env::temp_dir().join(format!("fliuno-acp-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&root).unwrap();
    let path = root.join("host.txt");
    std::fs::write(&path, "private").unwrap();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let probe = format!(
        "probe {}",
        serde_json::json!({
            "path":path,
            "address":listener.local_addr().unwrap().to_string(),
        })
    );
    let options = ClientOptions {
        request_timeout: Duration::from_secs(10),
        ..ClientOptions::default()
    };

    // Successful unsandboxed controls are necessary for meaningful denials.
    let mut native = tokio::process::Command::new(executable())
        .current_dir(&root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let (control, mut events) = Client::connect_io(
        native.stdout.take().unwrap(),
        native.stdin.take().unwrap(),
        options.clone(),
    )
    .await
    .unwrap();
    let session = control.new_session(&root).await.unwrap().session_id;
    assert_eq!(update(&mut events).await, "ready");
    control
        .prompt_text(&session, probe.clone(), &CancellationToken::new())
        .await
        .unwrap();
    let result: serde_json::Value = serde_json::from_str(&update(&mut events).await).unwrap();
    assert_eq!(
        result,
        serde_json::json!({"appContainer":false,"hostRead":true,"hostWrite":true,"hostTcp":true,"scratchWrite":true})
    );
    control.close().await;
    assert!(
        tokio::time::timeout(Duration::from_secs(5), native.wait())
            .await
            .unwrap()
            .unwrap()
            .success()
    );
    std::fs::write(&path, "private").unwrap();

    let (isolated, mut events) = Client::connect_isolated(
        fliuno_sandbox::LaunchSpec {
            program_sha256: None,
            assets: vec![],
            program: executable(),
            args: Vec::new(),
            env: BTreeMap::new(),
            resources: Default::default(),
            linux_cgroup_parent: None,
        },
        ClientOptions {
            prompt_timeout: Duration::from_secs(2),
            cancellation_grace: Duration::from_millis(200),
            ..options
        },
    )
    .await
    .unwrap();
    let private = isolated.private_directory().unwrap().to_owned();
    assert!(matches!(
        isolated.new_session(&root).await,
        Err(Error::InvalidInput(_))
    ));
    let session = isolated.new_session(&private).await.unwrap().session_id;
    assert!(matches!(
        isolated.list_sessions(Some(&root), None).await,
        Err(Error::InvalidInput(_))
    ));
    let page = isolated.list_sessions(None, None).await.unwrap();
    assert_eq!(page.sessions.len(), 1);
    assert_eq!(page.sessions[0].session_id, session);
    assert_eq!(page.sessions[0].cwd, private);
    assert_eq!(update(&mut events).await, "ready");
    isolated
        .prompt_text(&session, probe, &CancellationToken::new())
        .await
        .unwrap();
    let result: serde_json::Value = serde_json::from_str(&update(&mut events).await).unwrap();
    assert_eq!(
        result,
        serde_json::json!({"appContainer":true,"hostRead":false,"hostWrite":false,"hostTcp":false,"scratchWrite":true})
    );
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "private");
    assert!(private.join("probe.txt").is_file());

    assert_eq!(
        isolated
            .prompt_text(&session, "hang", &CancellationToken::new())
            .await
            .unwrap_err(),
        Error::OutcomeUnknown
    );
    assert!(isolated.is_closed());
    isolated.close().await;
    assert!(
        !private.exists(),
        "timeout must release the AppContainer profile"
    );
    std::fs::remove_dir_all(&root).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dropping_last_client_releases_owned_sandbox_even_if_event_receiver_survives() {
    let (client, mut events) = Client::connect_isolated(
        fliuno_sandbox::LaunchSpec {
            program_sha256: None,
            assets: vec![],
            program: executable(),
            args: Vec::new(),
            env: BTreeMap::new(),
            resources: Default::default(),
            linux_cgroup_parent: None,
        },
        ClientOptions {
            request_timeout: Duration::from_secs(10),
            ..ClientOptions::default()
        },
    )
    .await
    .unwrap();
    let directory = client.private_directory().unwrap().to_owned();
    drop(client);
    assert!(
        tokio::time::timeout(Duration::from_secs(5), events.recv())
            .await
            .unwrap()
            .is_none()
    );
    tokio::time::timeout(Duration::from_secs(5), async {
        while directory.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn official_acp_agent_reads_private_pinned_companion_after_source_changes() {
    use sha2::{Digest, Sha256};
    let root = std::env::temp_dir().join(format!("fliuno-acp-assets-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&root).unwrap();
    let source = root.join("source.txt");
    std::fs::write(&source, "审批时的内容").unwrap();
    let (client, mut events) = Client::connect_isolated(
        fliuno_sandbox::LaunchSpec {
            program_sha256: None,
            assets: vec![fliuno_sandbox::StagedFile {
                source: source.clone(),
                path: "data/approved.txt".into(),
                sha256: format!("{:x}", Sha256::digest("审批时的内容".as_bytes())),
                executable: false,
            }],
            program: executable(),
            args: vec![],
            env: BTreeMap::new(),
            resources: Default::default(),
            linux_cgroup_parent: None,
        },
        ClientOptions::default(),
    )
    .await
    .unwrap();
    std::fs::write(source, "source changed after staging").unwrap();
    let private = client.private_directory().unwrap().to_owned();
    let session = client.new_session(&private).await.unwrap().session_id;
    assert_eq!(update(&mut events).await, "ready");
    client
        .prompt_text(
            &session,
            "asset data/approved.txt",
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(update(&mut events).await, "审批时的内容");
    client.close().await;
    assert!(!private.exists());
    std::fs::remove_dir_all(root).unwrap();
}
