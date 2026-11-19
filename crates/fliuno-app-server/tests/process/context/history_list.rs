use super::*;

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
