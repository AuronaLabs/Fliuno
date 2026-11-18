use agent_client_protocol::{self as acp, v1::schema::*};
use fliuno_acp::{CancellationToken, Client, ClientOptions, Error, Event, PermissionDecision};
use std::time::Duration;
use tokio::io::AsyncWriteExt;

#[path = "../src/fixture.rs"]
mod fixture;

#[tokio::test]
async fn unsupported_version_and_initialization_timeout_fail_closed() {
    // The SDK correctly normalizes its own agent's version. Use a raw wire peer
    // to ensure a truly unsupported response reaches the client unchanged.
    let (local, peer) = tokio::io::duplex(4096);
    let (input, output) = tokio::io::split(local);
    let (peer_input, mut peer_output) = tokio::io::split(peer);
    let peer = tokio::spawn(async move {
        use tokio::io::AsyncBufReadExt;
        let mut line = String::new();
        tokio::io::BufReader::new(peer_input)
            .read_line(&mut line)
            .await
            .unwrap();
        let request: serde_json::Value = serde_json::from_str(&line).unwrap();
        let response = serde_json::json!({"jsonrpc":"2.0","id":request["id"],
            "result":{"protocolVersion":99,"agentCapabilities":{}}});
        peer_output
            .write_all(format!("{response}\n").as_bytes())
            .await
            .unwrap();
        std::future::pending::<()>().await;
    });
    let error = Client::connect_io(input, output, ClientOptions::default())
        .await
        .err()
        .unwrap();
    assert!(
        matches!(error, Error::Protocol(_) | Error::Remote(_)),
        "{error:?}"
    );
    peer.abort();
    let _ = peer.await;
    let hung = acp::Agent.v1().on_receive_request(
        async |_request: InitializeRequest, responder, cx| {
            cx.spawn(async move {
                let _responder = responder;
                std::future::pending::<()>().await;
                Ok(())
            })
        },
        acp::on_receive_request!(),
    );
    let options = ClientOptions {
        request_timeout: Duration::from_millis(50),
        ..ClientOptions::default()
    };
    assert_eq!(
        Client::connect(hung, options).await.err().unwrap(),
        Error::OutcomeUnknown
    );
}

#[tokio::test]
async fn oversized_unterminated_wire_line_is_rejected_before_unbounded_read() {
    let (local, peer) = tokio::io::duplex(4096);
    let (input, output) = tokio::io::split(local);
    let (mut peer_input, mut peer_output) = tokio::io::split(peer);
    let writer = tokio::spawn(async move {
        // Drain initialize so backpressure cannot be mistaken for rejection.
        let mut initialize = Vec::new();
        use tokio::io::AsyncBufReadExt;
        tokio::io::BufReader::new(&mut peer_input)
            .read_until(b'\n', &mut initialize)
            .await
            .unwrap();
        let _ = peer_output.write_all(&vec![b'x'; 1024 * 1024 + 1]).await;
        std::future::pending::<()>().await;
    });
    let options = ClientOptions {
        request_timeout: Duration::from_secs(2),
        ..ClientOptions::default()
    };
    let error = tokio::time::timeout(
        Duration::from_secs(3),
        Client::connect_io(input, output, options),
    )
    .await
    .unwrap()
    .err()
    .unwrap();
    assert_ne!(
        error,
        Error::OutcomeUnknown,
        "framing must fail before initialization's timeout"
    );
    writer.abort();
    let _ = writer.await;
}

#[tokio::test]
async fn permission_timeout_cancels_and_late_consent_is_rejected() {
    let options = ClientOptions {
        permission_timeout: Duration::from_millis(50),
        ..ClientOptions::default()
    };
    let (client, mut events) = Client::connect(fixture::agent(), options).await.unwrap();
    let session = client
        .new_session(std::env::current_dir().unwrap())
        .await
        .unwrap()
        .session_id;
    events.recv().await.unwrap(); // ready before response
    let task = tokio::spawn({
        let client = client.clone();
        let session = session.clone();
        async move {
            client
                .prompt_text(&session, "permission", &CancellationToken::new())
                .await
        }
    });
    let Event::PermissionRequested { id, .. } = events.recv().await.unwrap() else {
        panic!("expected permission")
    };
    let Some(Event::PermissionClosed { outcome, .. }) =
        tokio::time::timeout(Duration::from_secs(2), events.recv())
            .await
            .unwrap()
    else {
        panic!("expected closure")
    };
    assert_eq!(outcome, RequestPermissionOutcome::Cancelled);
    assert_eq!(
        client.respond_permission(&session, &id, PermissionDecision::AllowOnce),
        Err(Error::NotFound)
    );
    assert_eq!(
        task.await.unwrap().unwrap().stop_reason,
        StopReason::EndTurn
    );
    assert!(!client.is_closed());
    client.close().await;
}

#[tokio::test]
async fn already_cancelled_prompt_never_starts_remote_work() {
    let (client, mut events) = Client::connect(fixture::agent(), ClientOptions::default())
        .await
        .unwrap();
    let session = client
        .new_session(std::env::current_dir().unwrap())
        .await
        .unwrap()
        .session_id;
    events.recv().await.unwrap();
    let cancellation = CancellationToken::new();
    cancellation.cancel();
    assert_eq!(
        client
            .prompt_text(&session, "hang", &cancellation)
            .await
            .unwrap_err(),
        Error::Cancelled
    );
    assert!(!client.is_closed());
    client
        .prompt_text(&session, "usable", &CancellationToken::new())
        .await
        .unwrap();
    client.close().await;
}

#[tokio::test]
async fn permission_burst_is_bounded_before_waiters_spawn() {
    let (client, mut events) = Client::connect(fixture::agent(), ClientOptions::default())
        .await
        .unwrap();
    let session = client
        .new_session(std::env::current_dir().unwrap())
        .await
        .unwrap()
        .session_id;
    events.recv().await.unwrap();
    let task = tokio::spawn({
        let client = client.clone();
        let session = session.clone();
        async move {
            client
                .prompt_text(&session, "permissions_burst", &CancellationToken::new())
                .await
        }
    });
    let mut ids = Vec::new();
    loop {
        let event = tokio::time::timeout(Duration::from_secs(2), events.recv())
            .await
            .unwrap()
            .unwrap();
        match event {
            Event::PermissionRequested { id, .. } => ids.push(id),
            Event::Update(update) => {
                let SessionUpdate::AgentMessageChunk(chunk) = update.update else {
                    panic!("expected message")
                };
                let ContentBlock::Text(text) = chunk.content else {
                    panic!("expected text")
                };
                assert_eq!(text.text, "excess_cancelled:16");
                break;
            }
            _ => panic!("no request should resolve before a UI decision"),
        }
    }
    assert_eq!(ids.len(), 16);
    for id in ids {
        client
            .respond_permission(&session, &id, PermissionDecision::Deny)
            .unwrap();
    }
    for _ in 0..16 {
        assert!(matches!(
            events.recv().await,
            Some(Event::PermissionClosed { .. })
        ));
    }
    assert_eq!(
        task.await.unwrap().unwrap().stop_reason,
        StopReason::EndTurn
    );
    client.close().await;
}

#[tokio::test]
async fn mismatched_early_updates_and_empty_session_ids_are_rejected() {
    for mismatched in [false, true] {
        let agent = acp::Agent
            .v1()
            .on_receive_request(
                async |request: InitializeRequest, responder, _cx| {
                    responder.respond(InitializeResponse::new(request.protocol_version))
                },
                acp::on_receive_request!(),
            )
            .on_receive_request(
                async move |_request: NewSessionRequest, responder, cx| {
                    if mismatched {
                        cx.send_notification(SessionNotification::new(
                            "unexpected",
                            SessionUpdate::AgentMessageChunk(ContentChunk::new(
                                ContentBlock::Text(TextContent::new("unrelated")),
                            )),
                        ))?;
                    }
                    responder.respond(NewSessionResponse::new(if mismatched {
                        "expected"
                    } else {
                        ""
                    }))
                },
                acp::on_receive_request!(),
            );
        let (client, _events) = Client::connect(agent, ClientOptions::default())
            .await
            .unwrap();
        let error = client
            .new_session(std::env::current_dir().unwrap())
            .await
            .unwrap_err();
        assert!(matches!(error, Error::Protocol(_)), "{error:?}");
        assert!(client.is_closed());
        client.close().await;
    }
}
