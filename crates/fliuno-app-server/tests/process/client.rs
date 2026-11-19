use super::*;
use fliuno_client::{Client, Error, EventReplay, ReplayScope};
use fliuno_protocol::{
    ApprovalDecision, ApprovalGetParams, ApprovalRespondParams, ArtifactGetParams, ArtifactSummary,
    Event, EventKind, EventSubscription, Task, TaskIdParams, TaskStatus, Workspace,
    WorkspaceOpenParams,
};

#[test]
fn rust_client_can_pair_notifications_and_resolve_approval_over_stdio() {
    let root = std::env::temp_dir()
        .canonicalize()
        .unwrap()
        .join(format!("fliuno-rust-client-{}", uuid::Uuid::new_v4()));
    fs::create_dir(&root).unwrap();
    let child = Command::new(env!("CARGO_BIN_EXE_fliuno-app-server"))
        .arg("--db")
        .arg(root.join("kernel.db"))
        .env("FLIUNO_PROVIDER", "demo")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    struct Server(Child);
    impl Drop for Server {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let mut server = Server(child);
    let writer = server.0.stdin.take().unwrap();
    let reader = BufReader::new(server.0.stdout.take().unwrap());
    let mut sequences = Vec::new();
    let mut observe = |note: fliuno_client::Notification| {
        if let Some(sequence) = note.params["event"]["sequence"].as_u64() {
            sequences.push(sequence);
        }
    };
    let mut client = Client::connect(reader, writer, "rust-client-test", &mut observe).unwrap();
    assert_eq!(client.info().server_name, "fliuno-app-server");
    assert_eq!(client.info().capabilities["approvalDecisionLookup"], true);
    let invalid: Result<Value, _> = client.call(
        "task/get",
        &TaskIdParams {
            task_id: "absent".into(),
        },
        &mut observe,
    );
    assert!(matches!(invalid, Err(Error::Rpc { code: -32004, .. })));
    let workspace: Workspace = client
        .call(
            "workspace/open",
            &WorkspaceOpenParams {
                root: root.to_str().unwrap().into(),
            },
            &mut observe,
        )
        .unwrap();
    let session: fliuno_protocol::Session = client
        .call(
            "session/create",
            &json!({"workspaceId":workspace.id}),
            &mut observe,
        )
        .unwrap();
    let content = "中🌿\0\n".repeat(9_000);
    let task: Task = client
        .call(
            "task/start",
            &json!({"sessionId":session.id,"input":content}),
            &mut observe,
        )
        .unwrap();
    let task_params = TaskIdParams {
        task_id: task.id.clone(),
    };
    let deadline = Instant::now() + Duration::from_secs(10);
    let approval = loop {
        let current: Task = client.call("task/get", &task_params, &mut observe).unwrap();
        if let Some(approval) = current.pending_approval {
            break approval;
        }
        assert_eq!(current.status, TaskStatus::Running);
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(10));
    };
    let _: Task = client
        .call(
            "approval/respond",
            &ApprovalRespondParams {
                task_id: task.id.clone(),
                approval_id: approval.id.clone(),
                decision: ApprovalDecision::AllowOnce,
            },
            &mut observe,
        )
        .unwrap();
    let event: Event = client
        .call(
            "approval/getDecision",
            &ApprovalGetParams {
                task_id: task.id.clone(),
                approval_id: approval.id,
            },
            &mut observe,
        )
        .unwrap();
    assert!(matches!(
        event.data,
        EventKind::ApprovalResolved {
            decision: ApprovalDecision::AllowOnce,
            ..
        }
    ));
    loop {
        let current: Task = client.call("task/get", &task_params, &mut observe).unwrap();
        if current.status == TaskStatus::Completed {
            break;
        }
        assert_eq!(current.status, TaskStatus::Running);
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(10));
    }
    let summaries: fliuno_protocol::ObjectPage<ArtifactSummary> = client
        .call("artifacts/listSummaries", &task_params, &mut observe)
        .unwrap();
    assert_eq!(summaries.items.len(), 1);
    let artifact = ArtifactGetParams {
        task_id: task.id.clone(),
        artifact_id: summaries.items[0].id.clone(),
    };
    let expected = client.artifact_digest(&artifact, &mut observe).unwrap();
    assert_eq!(expected.content_bytes, content.len() as u64);
    let mut output = Vec::new();
    client
        .download_artifact(&artifact, &expected, &mut output, &mut observe)
        .unwrap();
    assert_eq!(output, content.as_bytes());
    let subscription: EventSubscription = client
        .call("events/setScope", &json!({"taskId":task.id}), &mut observe)
        .unwrap();
    let mut replay =
        EventReplay::new(ReplayScope::Task(task.id), 0, subscription.through_sequence).unwrap();
    let mut read = Vec::new();
    while let Some(page) = replay
        .next_page(&mut client, 1, Some(4096), &mut observe)
        .unwrap()
    {
        assert_eq!(page.len(), 1);
        read.push(page[0].sequence);
    }
    assert!(read.contains(&event.sequence));
    assert!(read.windows(2).all(|pair| pair[0] < pair[1]));
    assert_eq!(replay.cursor(), subscription.through_sequence);
    if sequences.is_empty() {
        let notification = client.read_notification().unwrap();
        assert_eq!(notification.method, "notifications/event");
        sequences.push(notification.params["event"]["sequence"].as_u64().unwrap());
    }
    assert!(sequences.windows(2).all(|pair| pair[0] < pair[1]));
    let (mut reader, writer) = client.into_parts();
    // Close stdin so shutdown begins, and keep stdout open for queued notifications.
    drop(writer);
    std::io::copy(&mut reader, &mut std::io::sink()).unwrap();
    assert!(server.0.wait().unwrap().success());
    fs::remove_dir_all(root).unwrap();
}
