#[path = "../src/fixture.rs"]
mod fixture;
use fliuno_acp::{CancellationToken, Client, ClientOptions, Event, HostRequest, HostResponse};
use std::time::Duration;

async fn next(events: &mut fliuno_acp::Events) -> Event {
    tokio::time::timeout(Duration::from_secs(3), events.recv())
        .await
        .unwrap()
        .unwrap()
}

#[tokio::test]
async fn filesystem_requests_bind_session_kind_and_expiry_without_implicit_io() {
    let (client, mut events) = Client::connect(
        fixture::agent(),
        ClientOptions {
            filesystem: true,
            permission_timeout: Duration::from_millis(500),
            ..ClientOptions::default()
        },
    )
    .await
    .unwrap();
    let session = client
        .new_session(std::env::current_dir().unwrap())
        .await
        .unwrap()
        .session_id;
    let other = client
        .new_session(std::env::current_dir().unwrap())
        .await
        .unwrap()
        .session_id;
    next(&mut events).await;
    next(&mut events).await; // Early ready updates.
    let task_client = client.clone();
    let task_session = session.clone();
    let prompt = tokio::spawn(async move {
        task_client
            .prompt_text(&task_session, "host_read", &CancellationToken::new())
            .await
    });
    let Event::HostRequest {
        id,
        request: HostRequest::Read(request),
        ..
    } = next(&mut events).await
    else {
        panic!("expected read request");
    };
    assert_eq!(request.line, Some(2));
    assert_eq!(request.limit, Some(1));
    assert!(
        client
            .respond_host_request(&other, &id, HostResponse::Read("wrong".into()))
            .is_err()
    );
    assert!(
        client
            .respond_host_request(&session, &id, HostResponse::Written)
            .is_err()
    );
    client
        .respond_host_request(&session, &id, HostResponse::Read("批准的文本\n".into()))
        .unwrap();
    assert!(
        client
            .respond_host_request(&session, &id, HostResponse::Read("duplicate".into()))
            .is_err()
    );
    assert!(prompt.await.unwrap().is_ok());
    while !matches!(next(&mut events).await, Event::Update(update) if serde_json::to_string(&update).unwrap().contains("批准的文本"))
    {
    }
    let task_client = client.clone();
    let task_session = session.clone();
    let prompt = tokio::spawn(async move {
        task_client
            .prompt_text(&task_session, "host_write", &CancellationToken::new())
            .await
    });
    let Event::HostRequest {
        id,
        request: HostRequest::Write(_),
        ..
    } = next(&mut events).await
    else {
        panic!("expected write request");
    };
    assert!(matches!(
        next(&mut events).await,
        Event::HostRequestClosed { .. }
    ));
    assert!(
        client
            .respond_host_request(&session, &id, HostResponse::Written)
            .is_err()
    );
    assert!(prompt.await.unwrap().is_ok());
    let cancellation = CancellationToken::new();
    let turn_cancel = cancellation.clone();
    let task_client = client.clone();
    let task_session = session.clone();
    let prompt = tokio::spawn(async move {
        task_client
            .prompt_text(&task_session, "host_read", &turn_cancel)
            .await
    });
    let id = loop {
        if let Event::HostRequest { id, .. } = next(&mut events).await {
            break id;
        }
    };
    cancellation.cancel();
    loop {
        if let Event::HostRequestClosed { id: closed, .. } = next(&mut events).await
            && closed == id
        {
            break;
        }
    }
    assert!(
        client
            .respond_host_request(&session, &id, HostResponse::Read("late".into()))
            .is_err()
    );
    assert!(prompt.await.unwrap().is_ok());
    client.close().await;
}

#[tokio::test]
async fn host_request_capacity_is_reserved_before_waiters_spawn() {
    let (client, mut events) = Client::connect(
        fixture::agent(),
        ClientOptions {
            filesystem: true,
            ..ClientOptions::default()
        },
    )
    .await
    .unwrap();
    let session = client
        .new_session(std::env::current_dir().unwrap())
        .await
        .unwrap()
        .session_id;
    next(&mut events).await;
    let task_client = client.clone();
    let task_session = session.clone();
    let prompt = tokio::spawn(async move {
        task_client
            .prompt_text(&task_session, "host_burst", &CancellationToken::new())
            .await
    });
    let mut requests = Vec::new();
    for _ in 0..8 {
        let Event::HostRequest { id, .. } = next(&mut events).await else {
            panic!("expected bounded read");
        };
        requests.push(id);
    }
    let Event::Update(update) = next(&mut events).await else {
        panic!("expected excess rejection");
    };
    assert!(
        serde_json::to_string(&update)
            .unwrap()
            .contains("host_excess_rejected:8")
    );
    for id in requests {
        client
            .respond_host_request(&session, &id, HostResponse::Read("accepted".into()))
            .unwrap();
    }
    assert!(prompt.await.unwrap().is_ok());
    client.close().await;
}
