//! Fill the bounded live queue inside one dispatch, then recover on that stdio connection.
use super::*;

#[test]
fn live_event_stream_can_resubscribe_and_catch_up_to_a_fixed_watermark() {
    let root = std::env::temp_dir().join(format!(
        "fliuno-event-stream-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir(&root).unwrap();
    let mut peer = Peer::start(&root.join("kernel.db"));
    let workspace = peer.call("workspace/open", json!({"root":root}));
    let session = peer.call("session/create", json!({"workspaceId":workspace["id"]}));
    assert_eq!(
        peer.request("events/resubscribe", json!({}))["error"]["code"],
        -32009
    );

    // The stdio loop handles a batch before returning to its notification
    // receiver. Each submission commits at least one durable event.
    let batch: Vec<_> = (0..72).map(|i| json!({"jsonrpc":"2.0","method":"task/start",
        "params":{"sessionId":session["id"],"input":format!("task {i}"),"context":{"mode":"isolated"}}})).collect();
    writeln!(peer.input, "{}", json!(batch)).unwrap();
    peer.input.flush().unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut live = Vec::new();
    let gap_after = loop {
        let frame = peer
            .output
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .unwrap();
        match frame["method"].as_str() {
            Some("notifications/event") => live.push(frame["params"]["event"].clone()),
            Some("notifications/eventGap") => {
                break frame["params"]["afterSequence"].as_u64().unwrap();
            }
            _ => panic!("unexpected frame: {frame}"),
        }
    };
    assert_eq!(live.len(), 64);
    assert_eq!(live.last().unwrap()["sequence"], gap_after);
    assert_eq!(
        peer.request("events/resubscribe", json!({"after":gap_after}))["error"]["code"],
        -32602
    );
    let subscription = peer.call("events/resubscribe", json!({}));
    let through = subscription["throughSequence"].as_u64().unwrap();
    assert!(through > gap_after);
    assert_eq!(
        peer.request("events/resubscribe", json!({}))["error"]["code"],
        -32009
    );

    // Commit new work before replay. The bounded replay should still stop at
    // the watermark returned when the replacement subscriber was installed.
    let newer = peer.call(
        "task/start",
        json!({"sessionId":session["id"],"input":"after gap","context":{"mode":"isolated"}}),
    );
    let mut after = gap_after;
    loop {
        let page = peer.call(
            "events/listAll",
            json!({"after":after,"through":through,"limit":13,"targetBytes":2048}),
        );
        let events = page["events"].as_array().unwrap();
        assert!(!events.is_empty());
        live.extend(events.iter().cloned());
        after = page["nextAfter"].as_u64().unwrap();
        if page["hasMore"] == false {
            break;
        }
    }
    assert_eq!(after, through);
    assert_eq!(
        json!(live),
        peer.call("events/listAll", json!({"through":through,"limit":1000}))["events"]
    );
    let newer_id = newer["id"].as_str().unwrap();
    let notification = peer.wait_for_event(newer_id, "task_started");
    assert!(
        notification["params"]["event"]["sequence"]
            .as_u64()
            .unwrap()
            > through
    );
    assert!(
        peer.notifications
            .iter()
            .filter(|frame| frame["method"] == "notifications/event")
            .all(|frame| { frame["params"]["event"]["sequence"].as_u64().unwrap() > through })
    );
    drop(peer);
    for entry in fs::read_dir(&root).unwrap() {
        fs::remove_file(entry.unwrap().path()).unwrap();
    }
    fs::remove_dir(root).unwrap();
}

#[test]
fn session_replay_scopes_the_global_cursor_across_upgrade_and_restart() {
    fn cancelled_task(peer: &mut Peer, session: &Value, input: &str) -> Value {
        let task = peer.call(
            "task/start",
            json!({"sessionId":session,"input":input,"context":{"mode":"isolated"}}),
        );
        let id = &task["id"];
        peer.wait_for_status(id.as_str().unwrap(), "waiting_approval");
        let stopped = peer.call("task/cancel", json!({"taskId":id}));
        assert_eq!(stopped["status"], "cancelled");
        id.clone()
    }

    let root = std::env::temp_dir().join(format!("fliuno-session-replay-{}", uuid::Uuid::new_v4()));
    fs::create_dir(&root).unwrap();
    let db = root.join("kernel.db");
    let mut peer = Peer::start(&db);
    let ws = peer.call("workspace/open", json!({"root":root}));
    let session = peer.call("session/create", json!({"workspaceId":ws["id"]}));
    let other = peer.call("session/create", json!({"workspaceId":ws["id"]}));
    let empty = peer.call("session/create", json!({"workspaceId":ws["id"]}));
    let session = &session["id"];
    let other = &other["id"];
    let first = cancelled_task(&mut peer, session, &"large".repeat(1024));
    cancelled_task(&mut peer, other, "interleaved");
    let second = cancelled_task(&mut peer, session, "second scoped item");
    let last_in_other = cancelled_task(&mut peer, other, "before the watermark");
    let global = peer.call("events/listAll", json!({}));
    let through = global["nextAfter"].as_u64().unwrap();
    assert_eq!(
        global["events"].as_array().unwrap().last().unwrap()["taskId"],
        last_in_other
    );
    let expected: Vec<_> = global["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|event| event["taskId"] == first || event["taskId"] == second)
        .cloned()
        .collect();
    drop(peer);
    {
        let old = rusqlite::Connection::open(&db).unwrap();
        old.execute_batch(
            "DROP TRIGGER session_event_index_insert;
            DROP TABLE session_event_index; PRAGMA user_version=15;",
        )
        .unwrap();
    }
    let mut peer = Peer::start(&db);
    let newer = cancelled_task(&mut peer, session, "after the watermark");
    let mut cursor = 0;
    let mut received = Vec::new();
    let mut saw_large_event = false;
    loop {
        let page = peer.call(
            "events/listSession",
            json!({"sessionId":session,
            "after":cursor,"through":through,"limit":2,"targetBytes":1024}),
        );
        let events = page["events"].as_array().unwrap();
        assert!((1..=2).contains(&events.len()));
        if serde_json::to_vec(events).unwrap().len() > 1024 {
            assert_eq!(events.len(), 1);
            saw_large_event = true;
        }
        let next = page["nextAfter"].as_u64().unwrap();
        assert!(next > cursor && next < through);
        assert_eq!(events.last().unwrap()["sequence"], next);
        cursor = next;
        received.extend(events.iter().cloned());
        if page["hasMore"] == false {
            break;
        }
    }
    assert!(saw_large_event);
    assert_eq!(received, expected);
    assert_eq!(
        peer.call(
            "events/listSession",
            json!({"sessionId":session,"after":cursor,"through":through})
        ),
        json!({"events":[],"nextAfter":cursor,"hasMore":false})
    );
    assert_eq!(
        peer.call("events/listSession", json!({"sessionId":empty["id"]}))["events"],
        json!([])
    );
    assert_eq!(
        peer.request("events/listSession", json!({"sessionId":"absent"}))["error"]["code"],
        -32004
    );
    assert_eq!(
        peer.request(
            "events/listSession",
            json!({"sessionId":session,"through":u64::MAX})
        )["error"]["code"],
        -32602
    );
    drop(peer);

    let mut peer = Peer::start(&db);
    let tail = peer.call(
        "events/listSession",
        json!({"sessionId":session,"after":through}),
    );
    assert_eq!(
        tail["events"],
        peer.call("events/list", json!({"taskId":newer}))["events"]
    );
    assert_eq!(tail["hasMore"], false);
    let event = expected.last().unwrap();
    assert_eq!(
        peer.call("events/get", json!({"sequence":event["sequence"]})),
        *event
    );
    for (sequence, code) in [(0, -32602), (u64::MAX, -32602), (i64::MAX as u64, -32004)] {
        assert_eq!(
            peer.request("events/get", json!({"sequence":sequence}))["error"]["code"],
            code
        );
    }
    drop(peer);
    for entry in fs::read_dir(&root).unwrap() {
        fs::remove_file(entry.unwrap().path()).unwrap();
    }
    fs::remove_dir(root).unwrap();
}
