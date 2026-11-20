use super::*;
use fliuno_protocol::{
    ApprovalInboxListParams, MAX_WORK_RECENT_TASKS, TaskStatus, TaskStatusCounts, WorkSnapshot,
    WorkSnapshotParams,
};

#[test]
fn work_snapshot_tracks_recovery_and_bootstraps_an_inbox_after_upgrade() {
    let root = std::env::temp_dir().join(format!("fliuno-work-snapshot-{}", uuid::Uuid::new_v4()));
    fs::create_dir_all(root.join("foreign")).unwrap();
    let db = root.join("kernel.db");
    let mut peer = Peer::start(&db);
    let workspace = peer.call("workspace/open", json!({"root":root}));
    let ws = workspace["id"].as_str().unwrap();
    let foreign = peer.call("workspace/open", json!({"root":root.join("foreign")}));
    let first = peer.call("session/create", json!({"workspaceId":ws}));
    let sibling = peer.call("session/create", json!({"workspaceId":ws}));
    let other = peer.call("session/create", json!({"workspaceId":foreign["id"]}));
    let first_id = first["id"].as_str().unwrap();
    let start = |peer: &mut Peer, scope: &Value| {
        let task = peer.call(
            "task/start",
            json!({"sessionId": scope["id"], "input":"build one artifact"}),
        );
        peer.wait_for_status(task["id"].as_str().unwrap(), "waiting_approval")
    };
    let complete = start(&mut peer, &first);
    peer.call("approval/respond", json!({"taskId":complete["id"],"approvalId":complete["pendingApproval"]["id"],"decision":"allow_once"}));
    peer.wait_for_status(complete["id"].as_str().unwrap(), "completed");
    let failed = start(&mut peer, &first);
    peer.call("approval/respond", json!({"taskId":failed["id"],"approvalId":failed["pendingApproval"]["id"],"decision":"deny"}));
    let cancelled = start(&mut peer, &first);
    peer.call("task/cancel", json!({"taskId":cancelled["id"]}));
    let waiting = start(&mut peer, &first);
    let later = start(&mut peer, &sibling);
    let foreign_task = start(&mut peer, &other);
    peer.call(
        "session/setArchived",
        json!({"sessionId":sibling["id"],"archived":true}),
    );
    drop(peer);

    // Recreate the last schema's layout without touching the existing task projection.
    rusqlite::Connection::open(&db)
        .unwrap()
        .execute_batch(
            "BEGIN;
         DROP TRIGGER work_status_insert;
         DROP TRIGGER work_status_update;
         DROP TRIGGER work_status_delete;
         DROP TABLE work_status_counts;
         PRAGMA user_version=41;
         COMMIT;",
        )
        .unwrap();

    let held = http_wire::Fixture::start(|_| http_wire::Response::hold());
    let mut peer = Peer::start_with_model_fixture(&db, Some(&held.url("/v1/")));
    let empty_session = peer.call("session/create", json!({"workspaceId":ws}));
    let empty: WorkSnapshot = serde_json::from_value(
        peer.call("work/getSnapshot", json!({"sessionId":empty_session["id"]})),
    )
    .unwrap();
    assert_eq!(empty.task_counts, TaskStatusCounts::default());
    assert!(empty.approvals.page.items.is_empty());
    let requested_empty = peer.call(
        "work/getSnapshot",
        json!({"sessionId":empty_session["id"],"recentTaskLimit":1}),
    );
    assert_eq!(requested_empty["recentTasks"], json!([]));
    let active = peer.call(
        "task/start",
        json!({"sessionId":first_id,"input":"hold this model request"}),
    );
    let active_id = active["id"].as_str().unwrap();
    peer.wait_for_event(active_id, "model_started");
    // ModelStarted is durable before the HTTP send. Let the fixture receive
    // the complete request so shutdown tests an in-flight model response.
    let deadline = Instant::now() + Duration::from_secs(5);
    while held.requests().is_empty() {
        assert!(
            Instant::now() < deadline,
            "model request did not reach fixture"
        );
        thread::sleep(Duration::from_millis(5));
    }
    let live = peer.call(
        "work/getSnapshot",
        json!({"workspaceId":ws,"approvalLimit":1}),
    );
    assert_eq!(
        live["taskCounts"],
        json!({"running":1,"waitingApproval":2,"interrupted":0,"completed":1,"cancelled":1,"failed":1})
    );
    assert_eq!(
        live["approvals"]["page"]["items"][0]["taskId"],
        waiting["id"]
    );
    assert!(live.get("recentTasks").is_none());
    for invalid in [
        json!({"recentTaskLimit":0}),
        json!({"recentTaskLimit":MAX_WORK_RECENT_TASKS+1}),
        json!({"workspaceId":ws,"sessionId":first_id}),
        json!({"approvalLimit":101}),
        json!({"approvalTargetBytes":1023}),
    ] {
        assert_eq!(
            peer.request("work/getSnapshot", invalid)["error"]["code"],
            -32602
        );
    }
    assert_eq!(
        peer.request("work/getSnapshot", json!({"workspaceId":"missing"}))["error"]["code"],
        -32004
    );
    drop(peer);
    drop(held);

    let (server, mut client) = super::ag_ui_run::connect(&db);
    assert_eq!(client.info().capabilities["workSnapshot"], true);
    let request = WorkSnapshotParams {
        workspace_id: Some(ws.into()),
        session_id: None,
        approval_limit: 1,
        approval_target_bytes: Some(1024),
        recent_task_limit: Some(3),
    };
    assert_eq!(client.info().capabilities["workSubscription"], true);
    assert_eq!(client.info().capabilities["workRecentTasks"], true);
    let fliuno_client::WorkSubscription {
        snapshot,
        inbox,
        mut events,
    } = client.subscribe_work(&request, |_| {}).unwrap();
    assert_eq!(
        snapshot.task_counts,
        TaskStatusCounts {
            running: 0,
            waiting_approval: 2,
            interrupted: 1,
            completed: 1,
            cancelled: 1,
            failed: 1
        }
    );
    assert_eq!(snapshot.approvals.page.items[0].task_id, waiting["id"]);
    let preview = snapshot.recent_tasks.as_ref().unwrap();
    assert_eq!(preview.len(), 3);
    assert_eq!(preview[0].id, active["id"]);
    assert_eq!(preview[0].status, TaskStatus::Interrupted);
    assert_eq!(preview[1].id, later["id"]); // archived sibling
    assert_eq!(preview[1].status, TaskStatus::WaitingApproval);
    assert_eq!(preview[2].id, waiting["id"]);
    let through = snapshot.approvals.snapshot.through_sequence;
    assert_eq!(events.cursor(), through);
    assert!(
        events
            .next_page(&mut client, 10, None, |_| {})
            .unwrap()
            .is_none()
    );
    let stamp = client.event_watermark(|_| {}).unwrap();
    assert_eq!(through, stamp);
    let checkpoint = serde_json::to_vec(inbox.next_request().unwrap()).unwrap();
    // Keep processing notices from this connection while reading the pinned inbox.
    let mut observe = |notice| {
        let _ = events.observe(notice);
    };
    let _: Value = client.call("approval/respond",&json!({"taskId":later["id"],"approvalId":later["pendingApproval"]["id"],"decision":"deny"}), &mut observe).unwrap();
    let saved: ApprovalInboxListParams = serde_json::from_slice(&checkpoint).unwrap();
    let mut resumed = fliuno_client::ApprovalInbox::new(saved);
    let page = resumed
        .next_page(&mut client, &mut observe)
        .unwrap()
        .unwrap();
    assert_eq!(page.snapshot, snapshot.approvals.snapshot);
    assert_eq!(page.page.items[0].task_id, later["id"]);
    assert!(
        resumed
            .next_page(&mut client, &mut observe)
            .unwrap()
            .is_none()
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut resolved = false;
    while !resolved {
        let _ = client
            .event_watermark(|notice| {
                let _ = events.observe(notice);
            })
            .unwrap();
        while let Some(batch) = events.next_page(&mut client, 1, None, |_| {}).unwrap() {
            resolved |= batch.iter().any(|event| {
                event.task_id == later["id"]
                    && matches!(event.data, EventKind::ApprovalResolved { .. })
            });
        }
        assert!(
            resolved || Instant::now() < deadline,
            "approval change missing from the work feed"
        );
    }
    let (current, _) = client.work_snapshot(&request, |_| {}).unwrap();
    assert_eq!(current.task_counts.waiting_approval, 1);
    assert_eq!(current.task_counts.failed, 2);
    assert_eq!(current.recent_tasks.unwrap()[1].status, TaskStatus::Failed);
    let by_session = WorkSnapshotParams {
        workspace_id: None,
        session_id: Some(first_id.into()),
        ..request.clone()
    };
    let (narrow, _) = client.work_snapshot(&by_session, |_| {}).unwrap();
    assert_eq!(narrow.task_counts.failed, 1);
    let preview = narrow.recent_tasks.unwrap();
    assert_eq!(preview.len(), 3);
    assert_eq!(preview[0].id, active["id"]);
    assert_eq!(preview[1].id, waiting["id"]);
    assert_eq!(preview[2].id, cancelled["id"]);
    let all = WorkSnapshotParams {
        workspace_id: None,
        ..request
    };
    let (global, _) = client.work_snapshot(&all, |_| {}).unwrap();
    assert_eq!(global.task_counts.waiting_approval, 2);
    assert_eq!(global.task_counts.failed, 2);
    let preview = global.recent_tasks.unwrap();
    assert_eq!(preview.len(), 3);
    assert_eq!(preview[0].id, active["id"]);
    assert_eq!(preview[1].id, foreign_task["id"]);
    assert_eq!(preview[2].id, later["id"]);
    let replay: fliuno_protocol::EventsPage = client
        .call(
            "events/listWorkspace",
            &json!({"workspaceId":ws,"after":through}),
            |_| {},
        )
        .unwrap();
    assert!(
        replay
            .events
            .iter()
            .any(|event| event.task_id == later["id"]
                && matches!(
                    event.data,
                    fliuno_protocol::EventKind::ApprovalResolved { .. }
                ))
    );
    drop(client);
    drop(server);
    fs::remove_dir_all(root).unwrap();
}

/// A later request in the same frame commits before the snapshot response is
/// written. Its notification must still follow that response in the new scope.
#[test]
fn work_subscription_delivers_only_post_snapshot_events_in_scope() {
    let root = std::env::temp_dir().join(format!("fliuno-work-handoff-{}", uuid::Uuid::new_v4()));
    fs::create_dir(&root).unwrap();
    let db = root.join("kernel.db");
    let mut peer = Peer::start(&db);
    let workspace = peer.call("workspace/open", json!({"root":root}));
    let selected = peer.call("session/create", json!({"workspaceId":workspace["id"]}));
    let foreign = peer.call("session/create", json!({"workspaceId":workspace["id"]}));
    let start = |peer: &mut Peer, session: &Value| {
        let task = peer.call(
            "task/start",
            json!({"sessionId":session["id"],"input":"create an artifact"}),
        );
        peer.wait_for_status(task["id"].as_str().unwrap(), "waiting_approval")
    };
    let selected_task = start(&mut peer, &selected);
    let foreign_task = start(&mut peer, &foreign);
    let old_watermark = peer.call("events/getWatermark", json!({}))["throughSequence"].clone();
    let first_id = peer.id + 1;
    peer.id += 5;
    writeln!(peer.input, "{}", json!([
        {"jsonrpc":"2.0","id":first_id,"method":"work/subscribe","params":{"sessionId":selected["id"],"recentTaskLimit":2}},
        {"jsonrpc":"2.0","id":first_id+1,"method":"work/subscribe","params":{"sessionId":selected["id"],"workspaceId":workspace["id"]}},
        {"jsonrpc":"2.0","id":first_id+2,"method":"work/subscribe","params":{"workspaceId":"missing"}},
        {"jsonrpc":"2.0","id":first_id+3,"method":"work/subscribe","params":{"approvalTargetBytes":1023}},
        {"jsonrpc":"2.0","id":first_id+4,"method":"work/subscribe","params":{"recentTaskLimit":0}},
        {"jsonrpc":"2.0","method":"task/cancel","params":{"taskId":foreign_task["id"]}},
        {"jsonrpc":"2.0","method":"task/cancel","params":{"taskId":selected_task["id"]}}
    ])).unwrap();
    peer.input.flush().unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    let reply = loop {
        let frame = peer
            .output
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .unwrap();
        if frame.is_array() {
            break frame;
        }
        // Any queued event before the batch response still belongs to the old stream.
        assert_ne!(frame["method"], "notifications/eventWatermark");
    };
    assert_eq!(reply.as_array().unwrap().len(), 5);
    assert_eq!(reply[0]["id"], first_id);
    let snapshot: WorkSnapshot = serde_json::from_value(reply[0]["result"].clone()).unwrap();
    let through = snapshot.approvals.snapshot.through_sequence;
    assert_eq!(through, old_watermark.as_u64().unwrap());
    assert_eq!(snapshot.task_counts.waiting_approval, 1);
    assert_eq!(snapshot.task_counts.cancelled, 0);
    assert_eq!(
        snapshot.approvals.page.items[0].task_id,
        selected_task["id"]
    );
    let preview = snapshot.recent_tasks.unwrap();
    assert_eq!(preview.len(), 1);
    assert_eq!(preview[0].id, selected_task["id"]);
    assert_eq!(preview[0].status, TaskStatus::WaitingApproval);
    for (i, code) in [-32602, -32004, -32602, -32602].into_iter().enumerate() {
        assert_eq!(reply[i + 1]["error"]["code"], code);
    }
    let sequence = loop {
        let frame = peer
            .output
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .unwrap();
        assert_ne!(
            frame["method"], "notifications/event",
            "the old full event stream was not detached"
        );
        if frame["method"] == "notifications/eventWatermark" {
            break frame["params"]["throughSequence"].as_u64().unwrap();
        }
    };
    assert_eq!(sequence, through + 2); // first commit was outside the scope
    let cancel_event = peer.call("events/get", json!({"sequence":sequence}));
    assert_eq!(cancel_event["taskId"], selected_task["id"]);
    assert_eq!(cancel_event["data"]["type"], "task_cancelled");
    let current = peer.call(
        "work/getSnapshot",
        json!({"sessionId":selected["id"],"recentTaskLimit":2}),
    );
    assert_eq!(
        current["approvals"]["snapshot"]["throughSequence"],
        sequence
    );
    assert_eq!(current["recentTasks"][0]["status"], "cancelled");
    assert_eq!(current["taskCounts"]["waitingApproval"], 0);
    assert_eq!(current["taskCounts"]["cancelled"], 1);
    assert_eq!(current["approvals"]["page"]["items"], json!([]));
    drop(peer);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn session_count_batch_tracks_transitions_and_workspace_ownership_across_restart() {
    let root = std::env::temp_dir().join(format!("fliuno-work-counts-{}", uuid::Uuid::new_v4()));
    let foreign = root.join("foreign");
    fs::create_dir_all(&foreign).unwrap();
    let db = root.join("kernel.db");
    let mut peer = Peer::start(&db);
    let workspace = peer.call("workspace/open", json!({"root":root}));
    let other = peer.call("workspace/open", json!({"root":foreign}));
    let main = peer.call("session/create", json!({"workspaceId":workspace["id"]}));
    let empty = peer.call("session/create", json!({"workspaceId":workspace["id"]}));
    let excluded = peer.call("session/create", json!({"workspaceId":other["id"]}));
    let request = json!({"workspaceId":workspace["id"],"sessionIds":[empty["id"],main["id"],excluded["id"],main["id"],"missing"]});
    let zero = serde_json::to_value(TaskStatusCounts::default()).unwrap();
    let initial = peer.call("work/getSessionCounts", request.clone());
    assert_eq!(initial["throughSequence"], 0);
    assert_eq!(
        initial["items"],
        json!([
            {"sessionId":empty["id"],"taskCounts":zero},
            {"sessionId":main["id"],"taskCounts":zero}, null,
            {"sessionId":main["id"],"taskCounts":zero}, null,
        ])
    );
    let task = peer.call(
        "task/startSummary",
        json!({"sessionId":main["id"],"input":"prepare one report"}),
    );
    let task_id = task["id"].as_str().unwrap();
    let waiting = peer.wait_for_status(task_id, "waiting_approval");
    let blocked = peer.call("work/getSessionCounts", request.clone());
    assert!(blocked["throughSequence"].as_u64().unwrap() > 0);
    assert_eq!(blocked["items"][1]["taskCounts"]["waitingApproval"], 1);
    assert_eq!(blocked["items"][1], blocked["items"][3]);
    peer.call("approval/respond", json!({"taskId":task_id,"approvalId":waiting["pendingApproval"]["id"],"decision":"allow_once"}));
    peer.wait_for_status(task_id, "completed");
    peer.call(
        "session/setArchived",
        json!({"sessionId":main["id"],"archived":true}),
    );
    let finished = peer.call("work/getSessionCounts", request.clone());
    let snapshot = peer.call("work/getSnapshot", json!({"sessionId":main["id"]}));
    assert_eq!(finished["items"][1]["taskCounts"], snapshot["taskCounts"]);
    assert_eq!(finished["items"][1]["taskCounts"]["completed"], 1);
    assert_eq!(finished["items"][1]["taskCounts"]["waitingApproval"], 0);
    assert_eq!(
        finished["throughSequence"],
        snapshot["approvals"]["snapshot"]["throughSequence"]
    );
    assert_eq!(
        peer.request(
            "work/getSessionCounts",
            json!({"workspaceId":"missing","sessionIds":[main["id"]]})
        )["error"]["code"],
        -32004
    );
    for invalid in [
        json!([]),
        json!(vec![main["id"].clone(); 65]),
        json!(["bad key"]),
    ] {
        assert_eq!(
            peer.request(
                "work/getSessionCounts",
                json!({"workspaceId":workspace["id"],"sessionIds":invalid})
            )["error"]["code"],
            -32602
        );
    }
    drop(peer);
    let mut reopened = Peer::start(&db);
    assert_eq!(reopened.call("work/getSessionCounts", request), finished);
    drop(reopened);
    fs::remove_dir_all(root).unwrap();
}
