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
