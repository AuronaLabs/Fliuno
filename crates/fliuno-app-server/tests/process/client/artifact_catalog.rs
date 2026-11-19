use super::*;
use fliuno_client::{ArtifactCatalog, EventFeed};
use fliuno_protocol::{ArtifactCatalogParams, ArtifactCatalogScope, ArtifactCatalogSnapshotParams};

fn waiting_task(peer: &mut Peer, session: &Value, input: &str) -> Value {
    let task = peer.call(
        "task/startSummary",
        json!({"sessionId":session["id"],"input":input}),
    );
    peer.wait_for_status(task["id"].as_str().unwrap(), "waiting_approval")
}

fn existing_artifact(peer: &mut Peer, session: &Value, input: &str) -> ArtifactSummary {
    let task = waiting_task(peer, session, input);
    let id = task["id"].as_str().unwrap();
    peer.call(
        "approval/respond",
        json!({"taskId":id,"approvalId":task["pendingApproval"]["id"],"decision":"allow_once"}),
    );
    peer.wait_for_status(id, "completed");
    let mut page: fliuno_protocol::ObjectPage<ArtifactSummary> =
        serde_json::from_value(peer.call("artifacts/listSummaries", json!({"taskId":id}))).unwrap();
    assert_eq!(page.items.len(), 1);
    page.items.pop().unwrap()
}

#[test]
fn artifact_subscription_separates_catalog_and_event_progress_across_restart() {
    let root = std::env::temp_dir().join(format!(
        "fliuno-artifact-subscribe-{}",
        uuid::Uuid::new_v4()
    ));
    fs::create_dir(&root).unwrap();
    let db = root.join("kernel.db");
    let mut peer = Peer::start(&db);
    let workspace = peer.call("workspace/open", json!({"root":root}));
    let selected = peer.call("session/create", json!({"workspaceId":workspace["id"]}));
    let foreign = peer.call("session/create", json!({"workspaceId":workspace["id"]}));
    let first = existing_artifact(&mut peer, &selected, "first artifact");
    let second = existing_artifact(&mut peer, &selected, "second artifact");
    existing_artifact(&mut peer, &foreign, "foreign catalog tail");
    let forthcoming = waiting_task(&mut peer, &selected, "live artifact");
    let noise = waiting_task(&mut peer, &foreign, "foreign live artifact");
    drop(peer);

    let (server, mut client) = super::ag_ui_run::connect(&db);
    let session_id = selected["id"].as_str().unwrap().to_owned();
    assert_eq!(
        client.info().capabilities["artifactCatalogSubscription"],
        true
    );
    let mut subscription = client
        .subscribe_artifacts(
            ArtifactCatalogParams {
                scope: ArtifactCatalogScope::Session {
                    session_id: session_id.clone(),
                },
                after: 0,
                limit: 1,
                media_type: Some(first.media_type.clone()),
            },
            |_| {},
        )
        .unwrap();
    assert_eq!(subscription.snapshot.catalog.items[0].id, first.id);
    assert!(subscription.snapshot.catalog.has_more);
    let pinned = subscription.snapshot.catalog.through_sequence;
    let event_base = subscription.snapshot.event_through_sequence;
    assert!(event_base > pinned);
    assert_eq!(subscription.events.cursor(), event_base);
    assert!(!subscription.catalog.begin_next_window());
    let saved_catalog = serde_json::to_vec(subscription.catalog.next_request().unwrap()).unwrap();

    let invalid: Result<Value, Error> = client.call(
        "artifacts/subscribe",
        &json!({
            "scope":{"kind":"workspace","workspaceId":workspace["id"]},"after":pinned+1
        }),
        |_| {},
    );
    assert!(matches!(invalid, Err(Error::Rpc { code: -32602, .. })));
    let [in_scope_end, global_end] = [&forthcoming, &noise].map(|task| {
        let _: Value = client.call("approval/respond", &json!({
            "taskId":task["id"],"approvalId":task["pendingApproval"]["id"],"decision":"allow_once"
        }), |notice| { let _ = subscription.events.observe(notice); }).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let current: TaskSummary = client
                .call("task/getSummary", &json!({"taskId":task["id"]}), |notice| {
                    let _ = subscription.events.observe(notice);
                })
                .unwrap();
            if current.status == TaskStatus::Completed {
                break;
            }
            assert!(Instant::now() < deadline && !current.status.is_terminal());
            thread::sleep(Duration::from_millis(5));
        }
        client
            .event_watermark(|notice| {
                let _ = subscription.events.observe(notice);
            })
            .unwrap()
    });
    assert!(global_end > in_scope_end);
    let forthcoming_id = forthcoming["id"].as_str().unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut live = None;
    while subscription.events.cursor() < in_scope_end {
        client
            .event_watermark(|notice| {
                let _ = subscription.events.observe(notice);
            })
            .unwrap();
        while let Some(events) = subscription
            .events
            .next_page(&mut client, 2, None, |_| {})
            .unwrap()
        {
            for event in events {
                assert!(event.sequence > event_base);
                assert_eq!(event.task_id, forthcoming_id);
                if let EventKind::ArtifactCreated { artifact_id } = event.data {
                    assert!(live.replace(artifact_id).is_none());
                }
            }
        }
        assert!(Instant::now() < deadline);
        if subscription.events.cursor() < in_scope_end {
            thread::sleep(Duration::from_millis(5));
        }
    }
    assert_eq!(subscription.events.cursor(), in_scope_end);
    assert!(live.is_some());
    drop(client);
    drop(server);

    let (server, mut client) = super::ag_ui_run::connect(&db);
    let mut replay = EventFeed::subscribe(
        &mut client,
        ReplayScope::Session(session_id),
        event_base,
        |_| {},
    )
    .unwrap();
    let mut catalog = ArtifactCatalog::new(
        serde_json::from_slice::<ArtifactCatalogSnapshotParams>(&saved_catalog).unwrap(),
    );
    let tail = catalog.next_page(&mut client, |_| {}).unwrap().unwrap();
    assert_eq!(tail.items[0].id, second.id);
    assert!(tail.next_after < pinned);
    assert_eq!(catalog.completed_through(), Some(pinned));
    assert!(catalog.next_request().is_none());

    assert!(catalog.begin_next_window());
    assert_eq!(catalog.next_request().unwrap().after, pinned);
    let next = catalog.next_page(&mut client, |_| {}).unwrap().unwrap();
    assert_eq!(next.items.len(), 1);
    let live = live.unwrap();
    assert_eq!(next.items[0].id, live);
    assert!(catalog.begin_next_window());
    let empty = catalog.next_page(&mut client, |_| {}).unwrap().unwrap();
    assert!(empty.items.is_empty());
    assert!(catalog.completed_through().is_some());

    let mut replayed = Vec::new();
    while let Some(events) = replay.next_page(&mut client, 3, None, |_| {}).unwrap() {
        for event in events {
            assert_eq!(event.task_id, forthcoming_id);
            if let EventKind::ArtifactCreated { artifact_id } = event.data {
                replayed.push(artifact_id);
            }
        }
    }
    assert_eq!(replayed, [live]);
    drop(client);
    drop(server);
    fs::remove_dir_all(root).unwrap();
}
