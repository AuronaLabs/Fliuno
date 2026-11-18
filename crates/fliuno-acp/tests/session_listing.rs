//! Exercise the stable v1 listSessions capability through the actual SDK.
use agent_client_protocol::{self as acp, Agent, ConnectTo};
use fliuno_acp::{CancellationToken, Client, ClientOptions, Error, schema::*};
use std::{
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

fn agent(cwd: PathBuf, advertised: bool, calls: Arc<AtomicUsize>) -> impl ConnectTo<acp::Client> {
    Agent
        .v1()
        .on_receive_request(
            async move |request: InitializeRequest, responder, _cx| {
                let mut response = InitializeResponse::new(request.protocol_version);
                if advertised {
                    response =
                        response.agent_capabilities(AgentCapabilities::new().session_capabilities(
                            SessionCapabilities::new().list(SessionListCapabilities::new()),
                        ));
                }
                responder.respond(response)
            },
            acp::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: ListSessionsRequest, responder, _cx| {
                calls.fetch_add(1, Ordering::Relaxed);
                assert_eq!(request.cwd.as_deref(), Some(cwd.as_path()));
                let item = SessionInfo::new("listed-session", cwd.clone());
                let page = match request.cursor.as_deref() {
                    None => ListSessionsResponse::new(vec![item.title("Work notes")])
                        .next_cursor("next"),
                    Some("next") => {
                        ListSessionsResponse::new(vec![SessionInfo::new("later", cwd.clone())])
                    }
                    Some("loop") => ListSessionsResponse::new(vec![]).next_cursor("loop"),
                    Some("duplicates") => ListSessionsResponse::new(vec![item.clone(), item]),
                    Some("too-many") => ListSessionsResponse::new(
                        (0..129)
                            .map(|n| SessionInfo::new(format!("session-{n}"), cwd.clone()))
                            .collect(),
                    ),
                    _ => {
                        return responder
                            .respond_with_error(acp::Error::new(-32091, "fixture-private"));
                    }
                };
                responder.respond(page)
            },
            acp::on_receive_request!(),
        )
}

#[tokio::test]
async fn a_catalog_page_preserves_cursor_and_does_not_open_listed_sessions() {
    let cwd = std::env::current_dir().unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let (client, _events) = Client::connect(
        agent(cwd.clone(), true, Arc::clone(&calls)),
        ClientOptions::default(),
    )
    .await
    .unwrap();
    assert!(client.supports_session_listing());
    assert!(matches!(
        client
            .list_sessions(Some(Path::new("relative")), None)
            .await,
        Err(Error::InvalidInput(_))
    ));
    assert!(matches!(
        client
            .list_sessions(Some(&cwd), Some(&"x".repeat(4097)))
            .await,
        Err(Error::InvalidInput(_))
    ));
    assert_eq!(calls.load(Ordering::Relaxed), 0);
    let first = client.list_sessions(Some(&cwd), None).await.unwrap();
    assert_eq!(first.sessions[0].title.as_deref(), Some("Work notes"));
    assert_eq!(calls.load(Ordering::Relaxed), 1);
    assert_eq!(
        client
            .prompt_text(
                &first.sessions[0].session_id,
                "prompt",
                &CancellationToken::new()
            )
            .await
            .unwrap_err(),
        Error::NotFound
    );
    let second = client
        .list_sessions(Some(&cwd), first.next_cursor.as_deref())
        .await
        .unwrap();
    assert_eq!(second.sessions[0].session_id, SessionId::new("later"));
    assert!(second.next_cursor.is_none());
    assert_eq!(calls.load(Ordering::Relaxed), 2);
    for cursor in ["loop", "duplicates", "too-many"] {
        assert!(matches!(
            client.list_sessions(Some(&cwd), Some(cursor)).await,
            Err(Error::Protocol(_))
        ));
    }
    assert_eq!(
        client
            .list_sessions(Some(&cwd), Some("peer-error"))
            .await
            .unwrap_err(),
        Error::Remote(-32091)
    );
    assert_eq!(calls.load(Ordering::Relaxed), 6);
    client.close().await;
}

#[tokio::test]
async fn an_unadvertised_catalog_sends_no_requests() {
    let cwd = std::env::current_dir().unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let (client, _events) = Client::connect(
        agent(cwd.clone(), false, Arc::clone(&calls)),
        ClientOptions::default(),
    )
    .await
    .unwrap();
    assert!(!client.supports_session_listing());
    assert_eq!(
        client.list_sessions(Some(&cwd), None).await.unwrap_err(),
        Error::Unsupported("session/list")
    );
    assert_eq!(calls.load(Ordering::Relaxed), 0);
    client.close().await;
}
