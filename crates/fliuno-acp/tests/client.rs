#[path = "../src/fixture.rs"]
mod fixture;

use fliuno_acp::{
    CancellationToken, Client, ClientOptions, Error, Event, Events, PermissionDecision, schema::*,
};
use std::time::Duration;

fn options() -> ClientOptions {
    ClientOptions {
        request_timeout: Duration::from_secs(2),
        prompt_timeout: Duration::from_secs(3),
        permission_timeout: Duration::from_secs(2),
        cancellation_grace: Duration::from_millis(100),
        event_capacity: 64,
        filesystem: false,
        terminal: false,
    }
}

async fn event(events: &mut Events) -> Event {
    tokio::time::timeout(Duration::from_secs(3), events.recv())
        .await
        .unwrap()
        .unwrap()
}

fn text(event: Event) -> String {
    let Event::Update(notification) = event else {
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

async fn session(client: &Client, events: &mut Events) -> SessionId {
    let session = client
        .new_session(std::env::current_dir().unwrap())
        .await
        .unwrap()
        .session_id;
    assert_eq!(text(event(events).await), "ready");
    session
}

async fn permission(events: &mut Events) -> (String, SessionId) {
    let Event::PermissionRequested { id, request, .. } = event(events).await else {
        panic!("expected permission")
    };
    (id, request.session_id)
}

#[tokio::test]
async fn sdk_v1_negotiates_streams_text_and_rejects_unadvertised_host_capabilities() {
    let (client, mut events) = Client::connect(fixture::agent(), options()).await.unwrap();
    let session = session(&client, &mut events).await;
    let result = client
        .prompt_text(&session, "ACP", &CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(result.stop_reason, StopReason::EndTurn);
    assert_eq!(text(event(&mut events).await), "你好，");
    assert_eq!(text(event(&mut events).await), "ACP");
    client
        .prompt_text(&session, "host_requests", &CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(text(event(&mut events).await), "host requests rejected");
    client.close().await;
    assert!(events.recv().await.is_none());
}

#[tokio::test]
async fn history_is_a_separate_bounded_data_block_and_invalid_prompts_do_not_claim_a_turn() {
    let (client, mut events) = Client::connect(fixture::agent(), options()).await.unwrap();
    let session = session(&client, &mut events).await;
    let cancellation = CancellationToken::new();
    for context in ["x".repeat(257 * 1024 + 1), "\0".repeat(257 * 1024)] {
        assert!(matches!(
            client
                .prompt_text_with_context(&session, "context_probe", Some(context), &cancellation)
                .await,
            Err(Error::InvalidInput(_))
        ));
    }
    client
        .prompt_text_with_context(
            &session,
            "context_probe",
            Some("x".repeat(257 * 1024)),
            &cancellation,
        )
        .await
        .unwrap();
    let probe: serde_json::Value = serde_json::from_str(&text(event(&mut events).await)).unwrap();
    assert_eq!(probe["current"], "context_probe");
    assert_eq!(probe["contextBytes"], 257 * 1024);
    assert!(probe["context"].is_null());
    client.close().await;
}

#[tokio::test]
async fn permissions_bind_to_session_expire_and_choose_kind_instead_of_label_or_position() {
    let (client, mut events) = Client::connect(fixture::agent(), options()).await.unwrap();
    let session = session(&client, &mut events).await;
    let other = self::session(&client, &mut events).await;
    let task = tokio::spawn({
        let client = client.clone();
        let session = session.clone();
        async move {
            client
                .prompt_text(&session, "permission", &CancellationToken::new())
                .await
        }
    });
    let (id, actual_session) = permission(&mut events).await;
    assert_eq!(
        client.close_session(&session).await.unwrap_err(),
        Error::Busy
    );
    client.close_session(&other).await.unwrap();
    assert_eq!(
        client.close_session(&other).await.unwrap_err(),
        Error::NotFound
    );
    assert_eq!(actual_session, session);
    assert_eq!(
        client.respond_permission(&other, &id, PermissionDecision::AllowOnce),
        Err(Error::NotFound)
    );
    assert_eq!(
        client
            .prompt_text(&session, "overlap", &CancellationToken::new())
            .await
            .unwrap_err(),
        Error::Busy
    );
    client
        .respond_permission(&session, &id, PermissionDecision::AllowOnce)
        .unwrap();
    assert_eq!(
        client.respond_permission(&session, &id, PermissionDecision::AllowOnce),
        Err(Error::NotFound)
    );
    let Event::PermissionClosed { outcome, .. } = event(&mut events).await else {
        panic!("expected permission closure")
    };
    assert_eq!(
        outcome,
        RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new("once"))
    );
    assert!(text(event(&mut events).await).contains("\"optionId\":\"once\""));
    assert_eq!(
        task.await.unwrap().unwrap().stop_reason,
        StopReason::EndTurn
    );
    client.close().await;
}

#[tokio::test]
async fn persistent_grants_are_never_substituted_for_single_use_consent() {
    let (client, mut events) = Client::connect(fixture::agent(), options()).await.unwrap();
    let session = session(&client, &mut events).await;
    let task = tokio::spawn({
        let client = client.clone();
        let session = session.clone();
        async move {
            client
                .prompt_text(&session, "persistent", &CancellationToken::new())
                .await
        }
    });
    let (id, _) = permission(&mut events).await;
    assert!(matches!(
        client.respond_permission(&session, &id, PermissionDecision::AllowOnce),
        Err(Error::InvalidInput(_))
    ));
    client
        .respond_permission(&session, &id, PermissionDecision::Deny)
        .unwrap();
    let Event::PermissionClosed { outcome, .. } = event(&mut events).await else {
        panic!("expected closure")
    };
    assert_eq!(outcome, RequestPermissionOutcome::Cancelled);
    assert!(text(event(&mut events).await).contains("cancelled"));
    task.await.unwrap().unwrap();
    client.close().await;
}

#[tokio::test]
async fn cancellation_closes_pending_permission_without_blocking_other_sessions() {
    let (client, mut events) = Client::connect(fixture::agent(), options()).await.unwrap();
    let session = session(&client, &mut events).await;
    let other = self::session(&client, &mut events).await;
    let task = tokio::spawn({
        let client = client.clone();
        let session = session.clone();
        async move {
            client
                .prompt_text(&session, "wait_cancel", &CancellationToken::new())
                .await
        }
    });
    let (id, _) = permission(&mut events).await;
    // The permission waiter is outside dispatch: another session still completes.
    client
        .prompt_text(&other, "other", &CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(text(event(&mut events).await), "你好，");
    assert_eq!(text(event(&mut events).await), "other");
    client.cancel(&session).unwrap();
    let Event::PermissionClosed { outcome, .. } = event(&mut events).await else {
        panic!("expected closure")
    };
    assert_eq!(outcome, RequestPermissionOutcome::Cancelled);
    assert!(text(event(&mut events).await).contains("cancelled"));
    assert_eq!(
        task.await.unwrap().unwrap().stop_reason,
        StopReason::Cancelled
    );
    assert_eq!(
        client.respond_permission(&session, &id, PermissionDecision::AllowOnce),
        Err(Error::NotFound)
    );
    assert!(!client.is_closed());
    client.close().await;
}

#[tokio::test]
async fn uncooperative_cancel_and_dropped_prompt_stop_connection_without_replay() {
    for abandon in [false, true] {
        let (client, mut events) = Client::connect(fixture::agent(), options()).await.unwrap();
        let session = session(&client, &mut events).await;
        let task = tokio::spawn({
            let client = client.clone();
            let session = session.clone();
            async move {
                client
                    .prompt_text(&session, "hang", &CancellationToken::new())
                    .await
            }
        });
        assert_eq!(text(event(&mut events).await), "waiting");
        if abandon {
            task.abort();
            assert!(task.await.unwrap_err().is_cancelled());
        } else {
            client.cancel(&session).unwrap();
            assert_eq!(task.await.unwrap().unwrap_err(), Error::OutcomeUnknown);
        }
        assert!(client.is_closed());
        assert_eq!(client.failure(), Some(Error::OutcomeUnknown));
        assert!(
            client
                .prompt_text(&session, "retry", &CancellationToken::new())
                .await
                .is_err()
        );
        client.close().await;
    }
}

#[tokio::test]
async fn notification_limits_and_consumer_loss_close_connection() {
    for mode in ["flood", "oversized"] {
        let mut options = options();
        options.event_capacity = 2;
        let (client, mut events) = Client::connect(fixture::agent(), options).await.unwrap();
        let session = session(&client, &mut events).await;
        assert!(
            client
                .prompt_text(&session, mode, &CancellationToken::new())
                .await
                .is_err()
        );
        assert!(client.is_closed());
        let failure = client.failure().unwrap();
        assert!(
            matches!(failure, Error::EventOverflow | Error::Protocol(_)),
            "{failure:?}"
        );
        client.close().await;
    }
    let (client, events) = Client::connect(fixture::agent(), options()).await.unwrap();
    drop(events);
    assert!(client.is_closed());
    client.close().await;
}

#[tokio::test]
async fn remote_private_errors_are_redacted_and_prompt_errors_close_session() {
    let (client, mut events) = Client::connect(fixture::agent(), options()).await.unwrap();
    let session = session(&client, &mut events).await;
    let error = client
        .prompt_text(&session, "error", &CancellationToken::new())
        .await
        .unwrap_err();
    assert_eq!(error, Error::Remote(-32099));
    assert!(!format!("{error:?} {error}").contains("synthetic-private-secret"));
    assert!(client.is_closed());
    client.close().await;
}
