//! Build an official v1 peer that streams history before acknowledging load.
use agent_client_protocol::{self as acp, Agent, ConnectTo};
use fliuno_acp::{CancellationToken, Client, ClientOptions, Error, Event, schema::*};
use std::{path::PathBuf, sync::Arc, time::Duration};
use tokio::sync::Notify;

fn update(id: SessionId, text: &str) -> SessionNotification {
    SessionNotification::new(
        id,
        SessionUpdate::AgentMessageChunk(ContentChunk::new(ContentBlock::Text(TextContent::new(
            text,
        )))),
    )
}

fn agent(cwd: PathBuf, ack: Arc<Notify>, advertised: bool) -> impl ConnectTo<acp::Client> {
    Agent
        .v1()
        .on_receive_request(
            async move |request: InitializeRequest, responder, _cx| {
                responder.respond(
                    InitializeResponse::new(request.protocol_version).agent_capabilities(
                        AgentCapabilities::new()
                            .load_session(advertised)
                            .session_capabilities(
                                SessionCapabilities::new()
                                    .close(advertised.then(SessionCloseCapabilities::new)),
                            ),
                    ),
                )
            },
            acp::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: LoadSessionRequest, responder, cx| {
                assert_eq!(request.cwd, cwd);
                assert!(request.mcp_servers.is_empty());
                assert!(request.additional_directories.is_empty());
                assert!(request.system_prompt.is_none());
                let cx_for_task = cx.clone();
                let ack = Arc::clone(&ack);
                cx.spawn(async move {
                    let id = request.session_id;
                    if id.0.as_ref() == "wrong-history" {
                        cx_for_task
                            .send_notification(update(SessionId::new("foreign"), "foreign"))?;
                        std::future::pending::<()>().await;
                    }
                    cx_for_task.send_notification(update(id.clone(), "history"))?;
                    if id.0.as_ref() == "overflow-history" {
                        cx_for_task.send_notification(update(id.clone(), "second"))?;
                        std::future::pending::<()>().await;
                    }
                    ack.notified().await;
                    let permission = cx_for_task
                        .send_request(RequestPermissionRequest::new(
                            id,
                            ToolCallUpdate::new("old_tool", ToolCallUpdateFields::new()),
                            vec![PermissionOption::new(
                                "allow",
                                "Allow",
                                PermissionOptionKind::AllowOnce,
                            )],
                        ))
                        .block_task()
                        .await?;
                    assert_eq!(permission.outcome, RequestPermissionOutcome::Cancelled);
                    responder.respond(LoadSessionResponse::new())
                })
            },
            acp::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: CloseSessionRequest, _responder, _cx| {
                assert_eq!(request.session_id, SessionId::new("slow-close"));
                std::future::pending::<Result<(), acp::Error>>().await
            },
            acp::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: PromptRequest, responder, cx| {
                cx.send_notification(update(request.session_id, "new turn"))?;
                responder.respond(PromptResponse::new(StopReason::EndTurn))
            },
            acp::on_receive_request!(),
        )
}

#[tokio::test]
async fn loaded_history_is_distinct_and_approval_needs_a_fresh_prompt() {
    let cwd = std::env::current_dir().unwrap();
    let ack = Arc::new(Notify::new());
    let (client, mut events) = Client::connect(
        agent(cwd.clone(), Arc::clone(&ack), true),
        ClientOptions::default(),
    )
    .await
    .unwrap();
    let id = SessionId::new("existing");
    assert!(client.supports_session_loading());
    let load = tokio::spawn({
        let (client, id, cwd) = (client.clone(), id.clone(), cwd.clone());
        async move { client.load_session(&id, cwd).await }
    });
    let Some(Event::SessionHistory(update)) =
        tokio::time::timeout(Duration::from_secs(2), events.recv())
            .await
            .unwrap()
    else {
        panic!("expected explicitly marked session history");
    };
    assert_eq!(update.session_id, id);
    assert_eq!(
        client
            .prompt_text(&id, "too early", &CancellationToken::new())
            .await
            .unwrap_err(),
        Error::NotFound
    );
    assert_eq!(client.new_session(&cwd).await.unwrap_err(), Error::Busy);
    ack.notify_one();
    tokio::time::timeout(Duration::from_secs(2), load)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(
        client.load_session(&id, &cwd).await.unwrap_err(),
        Error::Busy
    );
    assert_eq!(
        client
            .prompt_text(&id, "subsequent", &CancellationToken::new())
            .await
            .unwrap()
            .stop_reason,
        StopReason::EndTurn
    );
    let Some(Event::Update(update)) = tokio::time::timeout(Duration::from_secs(2), events.recv())
        .await
        .unwrap()
    else {
        panic!("expected only the live update, with no historical permission");
    };
    assert_eq!(update.session_id, id);
    client.close().await;
}

#[tokio::test]
async fn wrong_session_history_closes_the_peer_and_no_capability_sends_nothing() {
    let cwd = std::env::current_dir().unwrap();
    let id = SessionId::new("wrong-history");
    let ack = Arc::new(Notify::new());
    let (client, mut events) = Client::connect(
        agent(cwd.clone(), Arc::clone(&ack), true),
        ClientOptions::default(),
    )
    .await
    .unwrap();
    let result = tokio::time::timeout(Duration::from_secs(2), client.load_session(&id, &cwd))
        .await
        .unwrap();
    assert!(matches!(result, Err(Error::Protocol(_))));
    assert!(client.is_closed());
    assert!(events.recv().await.is_none());
    client.close().await;

    let (client, _events) =
        Client::connect(agent(cwd.clone(), ack, false), ClientOptions::default())
            .await
            .unwrap();
    assert_eq!(
        client.load_session(&id, &cwd).await.unwrap_err(),
        Error::Unsupported("session/load")
    );
    assert!(!client.is_closed());
    client.close().await;
}

#[tokio::test]
async fn an_unconsumed_replay_fails_instead_of_exceeding_the_event_queue() {
    let cwd = std::env::current_dir().unwrap();
    let (client, mut events) = Client::connect(
        agent(cwd.clone(), Arc::new(Notify::new()), true),
        ClientOptions {
            event_capacity: 1,
            ..ClientOptions::default()
        },
    )
    .await
    .unwrap();
    let id = SessionId::new("overflow-history");
    let error = tokio::time::timeout(Duration::from_secs(2), client.load_session(&id, &cwd))
        .await
        .unwrap()
        .unwrap_err();
    assert_eq!(error, Error::EventOverflow);
    assert!(client.is_closed());
    let Some(Event::SessionHistory(update)) = events.recv().await else {
        panic!("expected queued history")
    };
    assert_eq!(update.session_id, id);
    assert!(events.recv().await.is_none());
    client.close().await;
}

#[tokio::test]
async fn uncertain_close_retires_the_connection() {
    let cwd = std::env::current_dir().unwrap();
    let ack = Arc::new(Notify::new());
    ack.notify_one();
    let (client, mut events) = Client::connect(
        agent(cwd.clone(), ack, true),
        ClientOptions {
            request_timeout: Duration::from_millis(250),
            ..ClientOptions::default()
        },
    )
    .await
    .unwrap();
    let id = SessionId::new("slow-close");
    client.load_session(&id, &cwd).await.unwrap();
    assert!(matches!(
        events.recv().await,
        Some(Event::SessionHistory(_))
    ));
    assert_eq!(
        client.close_session(&id).await.unwrap_err(),
        Error::OutcomeUnknown
    );
    assert!(client.is_closed());
    assert_eq!(client.failure(), Some(Error::OutcomeUnknown));
    assert_eq!(
        client
            .prompt_text(&id, "closed", &CancellationToken::new())
            .await
            .unwrap_err(),
        Error::OutcomeUnknown
    );
    client.close().await;
}
