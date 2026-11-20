use super::*;

#[test]
fn session_history_subscription_hands_off_live_events_and_resumes_pages() {
    use fliuno_client::{Error, SessionHistory};
    use fliuno_protocol::{EventKind, SessionHistoryEntry, SessionHistorySubscribeParams};

    let root = std::env::temp_dir().join(format!("fliuno-history-live-{}", uuid::Uuid::new_v4()));
    fs::create_dir(&root).unwrap();
    let db = root.join("kernel.db");
    let mut peer = Peer::start(&db);
    let workspace = peer.call("workspace/open", json!({"root":root}));
    let selected = peer.call("session/create", json!({"workspaceId":workspace["id"]}));
    let other = peer.call("session/create", json!({"workspaceId":workspace["id"]}));
    approve_report(&mut peer, &selected, "Excluded prior history", "prior");
    let after = peer.call("events/getWatermark", json!({}))["throughSequence"]
        .as_u64()
        .unwrap();
    let first = approve_report(&mut peer, &selected, "First in this window", "first");
    let second = approve_report(&mut peer, &selected, "Second in this window", "second");
    let start = |peer: &mut Peer, session: &Value| {
        let task = peer.call(
            "task/start",
            json!({"sessionId":session["id"],"input":"Complete after subscribing"}),
        );
        peer.wait_for_status(task["id"].as_str().unwrap(), "waiting_approval")
    };
    let upcoming = start(&mut peer, &selected);
    let foreign = start(&mut peer, &other);
    drop(peer);

    let (server, mut client) = super::history_search::connect(&db);
    assert_eq!(
        client.info().capabilities["sessionHistorySubscription"],
        true
    );
    assert_eq!(
        client.info().capabilities["sessionHistoryAfterSequence"],
        true
    );
    let request = SessionHistorySubscribeParams {
        session_id: selected["id"].as_str().unwrap().to_owned(),
        after_sequence: Some(after),
        limit: Some(1),
        max_bytes: None,
    };
    let mut subscription = client
        .subscribe_session_history(request.clone(), |_| {})
        .unwrap();
    let [SessionHistoryEntry::Turn { turn }] = subscription.page.entries.as_slice() else {
        panic!("expected one compact history turn");
    };
    assert_eq!(turn.task_id, second["id"]);
    let through = subscription.page.through_sequence;
    assert_eq!(subscription.events.cursor(), through);
    let checkpoint = serde_json::to_value(subscription.history.next_request().unwrap()).unwrap();
    assert_eq!(checkpoint["afterSequence"], after);
    assert_eq!(checkpoint["beforeSequence"], turn.completion_sequence);
    assert_eq!(checkpoint["throughSequence"], through);

    let rejected = SessionHistorySubscribeParams {
        session_id: other["id"].as_str().unwrap().to_owned(),
        after_sequence: Some(through + 1),
        ..request.clone()
    };
    let mut observe = |note| {
        let _ = subscription.events.observe(note);
    };
    assert!(matches!(
        client.subscribe_session_history(rejected, &mut observe),
        Err(Error::Rpc { code: -32602, .. })
    ));
    for task in [&foreign, &upcoming] {
        let _: Value = client.call("approval/respond", &json!({"taskId":task["id"],"approvalId":task["pendingApproval"]["id"],"decision":"allow_once"}), &mut observe).unwrap();
    }
    let deadline = Instant::now() + Duration::from_secs(10);
    let live_completion = loop {
        assert!(
            Instant::now() < deadline,
            "live completion did not follow history snapshot"
        );
        client
            .event_watermark(|note| {
                let _ = subscription.events.observe(note);
            })
            .unwrap();
        let mut completion = None;
        while let Some(events) = subscription
            .events
            .next_page(&mut client, 16, None, |_| {})
            .unwrap()
        {
            for event in events {
                assert_eq!(event.task_id, upcoming["id"]);
                assert!(event.sequence > through);
                if matches!(event.data, EventKind::TaskCompleted { .. }) {
                    completion = Some(event.sequence);
                }
            }
        }
        if let Some(sequence) = completion {
            break sequence;
        }
        thread::sleep(Duration::from_millis(5));
    };
    drop(client);
    drop(server);

    let (server, mut client) = super::history_search::connect(&db);
    let mut history = SessionHistory::new(serde_json::from_value(checkpoint).unwrap());
    let tail = history.next_page(&mut client, |_| {}).unwrap().unwrap();
    assert_eq!(tail.through_sequence, through);
    let [SessionHistoryEntry::Turn { turn }] = tail.entries.as_slice() else {
        panic!("expected the remaining turn in the original window");
    };
    assert_eq!(turn.task_id, first["id"]);
    assert!(history.next_request().is_none());
    let delta = client
        .subscribe_session_history(
            SessionHistorySubscribeParams {
                after_sequence: Some(through),
                ..request.clone()
            },
            |_| {},
        )
        .unwrap();
    let [SessionHistoryEntry::Turn { turn }] = delta.page.entries.as_slice() else {
        panic!("expected just the new completion after restart");
    };
    assert_eq!(turn.task_id, upcoming["id"]);
    assert_eq!(turn.completion_sequence, live_completion);
    assert!(delta.history.next_request().is_none());
    assert_eq!(delta.events.cursor(), delta.page.through_sequence);
    let empty = client
        .subscribe_session_history(
            SessionHistorySubscribeParams {
                after_sequence: Some(delta.page.through_sequence),
                ..request
            },
            |_| {},
        )
        .unwrap();
    assert!(empty.page.entries.is_empty());
    assert!(empty.history.next_request().is_none());
    drop(client);
    drop(server);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn session_history_subscription_orders_batch_response_before_scoped_notifications() {
    let root = std::env::temp_dir().join(format!("fliuno-history-batch-{}", uuid::Uuid::new_v4()));
    fs::create_dir(&root).unwrap();
    let mut peer = Peer::start(&root.join("kernel.db"));
    let workspace = peer.call("workspace/open", json!({"root":root}));
    let selected = peer.call("session/create", json!({"workspaceId":workspace["id"]}));
    let other = peer.call("session/create", json!({"workspaceId":workspace["id"]}));
    let [included, excluded] = [&selected, &other].map(|session| {
        let task = peer.call(
            "task/start",
            json!({"sessionId":session["id"],"input":"Report"}),
        );
        peer.wait_for_status(task["id"].as_str().unwrap(), "waiting_approval")
    });
    let through = peer.call("events/getWatermark", json!({}))["throughSequence"]
        .as_u64()
        .unwrap();
    let first_id = peer.id + 1;
    peer.id += 2;
    writeln!(peer.input, "{}", json!([
        {"jsonrpc":"2.0","id":first_id,"method":"session/subscribeHistory","params":{"sessionId":selected["id"]}},
        {"jsonrpc":"2.0","id":first_id+1,"method":"session/subscribeHistory","params":{"sessionId":other["id"],"maxBytes":1023}},
        {"jsonrpc":"2.0","method":"task/cancel","params":{"taskId":excluded["id"]}},
        {"jsonrpc":"2.0","method":"task/cancel","params":{"taskId":included["id"]}}
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
        assert_ne!(frame["method"], "notifications/eventWatermark");
    };
    assert_eq!(reply[0]["id"], first_id);
    assert_eq!(
        reply[0]["result"],
        json!({"entries":[],"throughSequence":through,"nextBeforeSequence":null})
    );
    assert_eq!(reply[1]["error"]["code"], -32602);
    let frame = peer
        .output
        .recv_timeout(deadline.saturating_duration_since(Instant::now()))
        .unwrap();
    assert_eq!(frame["method"], "notifications/eventWatermark");
    let sequence = frame["params"]["throughSequence"].as_u64().unwrap();
    assert!(sequence > through);
    let event = peer.call("events/get", json!({"sequence":sequence}));
    assert_eq!(event["taskId"], included["id"]);
    assert_eq!(event["data"]["type"], "task_cancelled");
    drop(peer);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn pages_actual_completions_after_restart() {
    let root = std::env::temp_dir().join(format!("fliuno-history-list-{}", uuid::Uuid::new_v4()));
    fs::create_dir(&root).unwrap();
    let db = root.join("kernel.db");
    let mut peer = Peer::start(&db);
    let workspace = peer.call("workspace/open", json!({"root":root}));
    let session = peer.call("session/create", json!({"workspaceId":workspace["id"]}));
    let other = peer.call("session/create", json!({"workspaceId":workspace["id"]}));
    let pending = peer.call(
        "task/start",
        json!({"sessionId":session["id"],"input":"Created first, completed last"}),
    );
    let id = pending["id"].as_str().unwrap();
    let waiting = peer.wait_for_status(id, "waiting_approval");
    let older = approve_report(&mut peer, &session, "First completion", "old");
    let newer = approve_report(&mut peer, &session, "Second completion", "new");
    approve_report(&mut peer, &other, "Other session", "other");

    let method = "session/listHistory";
    let request = json!({"sessionId":session["id"],"limit":1});
    let page = peer.call(method, request.clone());
    let compact: fliuno_protocol::SessionHistoryListPage =
        serde_json::from_value(page.clone()).unwrap();
    assert_eq!(compact.entries.len(), 1);
    let turn = &page["entries"][0]["turn"];
    assert_eq!(page["entries"][0]["kind"], "turn");
    assert_eq!(turn["taskId"], newer["id"]);
    assert_eq!(turn["completionSequence"], page["nextBeforeSequence"]);
    assert_eq!(
        turn["messages"],
        json!([
            {"role":"user","text":"Second completion"},
            {"role":"assistant","text":"Demo artifact created."}
        ])
    );
    assert_eq!(turn["hostTools"], json!([]));
    let artifacts = peer.call("artifacts/list", json!({"taskId":newer["id"]}));
    assert_eq!(turn["artifacts"][0]["id"], artifacts[0]["id"]);

    let lookup = "session/getHistoryTurn";
    let mut lookup_params = json!({"sessionId":session["id"],"taskId":newer["id"],"throughSequence":page["throughSequence"]});
    assert_eq!(peer.call(lookup, lookup_params.clone()), page["entries"][0]);
    for (session_id, task_id) in [
        (&other["id"], &newer["id"]),
        (&session["id"], &pending["id"]),
        (&session["id"], &json!("missing")),
    ] {
        assert_eq!(
            peer.request(lookup, json!({"sessionId":session_id,"taskId":task_id}))["error"]["code"],
            -32004
        );
    }
    assert_eq!(
        peer.request(
            lookup,
            json!({"sessionId":session["id"],"taskId":newer["id"],"throughSequence":u64::MAX})
        )["error"]["code"],
        -32602
    );

    let decision =
        json!({"taskId":id,"approvalId":waiting["pendingApproval"]["id"],"decision":"allow_once"});
    peer.call("approval/respond", decision);
    peer.wait_for_status(id, "completed");
    drop(peer);

    let mut peer = Peer::start(&db);
    assert_eq!(
        peer.call(method, request.clone())["entries"][0]["turn"]["taskId"],
        id
    );
    let mut frozen = request;
    frozen["throughSequence"] = page["throughSequence"].clone();
    assert_eq!(peer.call(method, frozen.clone()), page);
    assert_eq!(peer.call(lookup, lookup_params.clone()), page["entries"][0]);
    lookup_params["taskId"] = json!(id);
    assert_eq!(peer.request(lookup, lookup_params)["error"]["code"], -32004);
    frozen["beforeSequence"] = page["nextBeforeSequence"].clone();
    let last = peer.call(method, frozen.clone());
    assert_eq!(last["entries"][0]["turn"]["taskId"], older["id"]);
    assert!(last["nextBeforeSequence"].is_null());
    assert_eq!(last["throughSequence"], page["throughSequence"]);

    let mut invalid = frozen.clone();
    invalid["beforeSequence"] = json!(0);
    assert_eq!(peer.request(method, invalid)["error"]["code"], -32602);
    frozen.as_object_mut().unwrap().remove("throughSequence");
    assert_eq!(peer.request(method, frozen)["error"]["code"], -32602);
    assert_eq!(
        peer.request(
            method,
            json!({"sessionId":session["id"],"throughSequence":u64::MAX})
        )["error"]["code"],
        -32602
    );
    assert_eq!(
        peer.request(method, json!({"sessionId":"missing"}))["error"]["code"],
        -32004
    );
    let empty = peer.call(
        method,
        json!({"sessionId":session["id"],"throughSequence":0}),
    );
    assert_eq!(
        empty,
        json!({"throughSequence":0,"entries":[],"nextBeforeSequence":null})
    );
    drop(peer);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn byte_budget_defers_a_turn_instead_of_dropping_or_truncating_it() {
    let root = std::env::temp_dir().join(format!("fliuno-history-budget-{}", uuid::Uuid::new_v4()));
    fs::create_dir(&root).unwrap();
    let db = root.join("kernel.db");
    let mut peer = Peer::start(&db);
    let workspace = peer.call("workspace/open", json!({"root":root}));
    let session = peer.call("session/create", json!({"workspaceId":workspace["id"]}));
    let escaped_utf8 = "界\"\n".repeat(60);
    let oldest = approve_report(&mut peer, &session, &escaped_utf8, "oldest");
    let middle = approve_report(&mut peer, &session, &escaped_utf8, "middle");
    let largest = approve_report(&mut peer, &session, &"界".repeat(600), "large");
    let method = "session/listHistory";
    let mut request = json!({"sessionId":session["id"],"maxBytes":1024});
    let first = peer.call(method, request.clone());
    let entries = first["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0]["kind"], "oversized");
    assert_eq!(entries[0]["taskId"], largest["id"]);
    let mut lookup = json!({"sessionId":session["id"],"taskId":largest["id"],"throughSequence":first["throughSequence"],"maxBytes":1024});
    assert_eq!(
        peer.call("session/getHistoryTurn", lookup.clone()),
        entries[0]
    );
    lookup["maxBytes"] = json!(4096);
    let read = peer.call("session/getHistoryTurn", lookup.clone());
    assert_eq!(read["turn"]["messages"][0]["text"], "界".repeat(600));
    assert_eq!(
        read["turn"]["completionSequence"],
        entries[0]["completionSequence"]
    );
    assert!(serde_json::to_vec(&read).unwrap().len() <= 4096);
    lookup["maxBytes"] = json!(1023);
    assert_eq!(
        peer.request("session/getHistoryTurn", lookup)["error"]["code"],
        -32602
    );
    assert!(
        entries[0]["completionSequence"].as_u64().unwrap()
            > first["nextBeforeSequence"].as_u64().unwrap()
    );
    assert_eq!(entries[1]["turn"]["taskId"], middle["id"]);
    assert_eq!(entries[1]["turn"]["messages"][0]["text"], escaped_utf8);
    assert_eq!(
        entries[1]["turn"]["completionSequence"],
        first["nextBeforeSequence"]
    );
    assert!(serde_json::to_vec(entries).unwrap().len() <= 1024);

    request["throughSequence"] = first["throughSequence"].clone();
    let mut retried = request.clone();
    retried["maxBytes"] = json!(4096);
    let expanded = peer.call(method, retried);
    assert_eq!(expanded["entries"][0]["kind"], "turn");
    assert_eq!(expanded["entries"][0]["turn"]["taskId"], largest["id"]);
    request["beforeSequence"] = first["nextBeforeSequence"].clone();
    let next = peer.call(method, request.clone());
    assert_eq!(next["entries"].as_array().unwrap().len(), 1);
    assert_eq!(next["entries"][0]["turn"]["taskId"], oldest["id"]);
    assert!(next["nextBeforeSequence"].is_null());
    request["maxBytes"] = json!(1023);
    assert_eq!(peer.request(method, request)["error"]["code"], -32602);
    drop(peer);
    fs::remove_dir_all(root).unwrap();
}
