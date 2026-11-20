//! Verify context windows across preview, task creation and restart.
use super::*;

#[test]
fn context_after_sequence_applies_to_all_selection_modes_and_survives_restart() {
    let temp_dir = std::env::temp_dir().canonicalize().unwrap();
    let root = temp_dir.join(format!("fliuno-context-window-{}", uuid::Uuid::new_v4()));
    fs::create_dir(&root).unwrap();
    let db = root.join("kernel.db");
    let mut peer = Peer::start(&db);
    let workspace = peer.call("workspace/open", json!({"root":root}));
    let session = peer.call("session/create", json!({"workspaceId":workspace["id"]}));
    let first = approve_report(&mut peer, &session, "needle before the handoff", "first");
    let after = peer.call("session/context", json!({"sessionId":session["id"]}))["turns"][0]
        ["completionSequence"].clone();
    let second = approve_report(&mut peer, &session, "needle after the handoff", "second");
    let before_migration = peer.call("session/context", json!({"sessionId":session["id"]}));
    let through = before_migration["throughSequence"].clone();
    drop(peer);

    // These snapshots were created without the new options shape.
    rusqlite::Connection::open(&db)
        .unwrap()
        .pragma_update(None, "user_version", 48)
        .unwrap();
    let mut peer = Peer::start(&db);
    let options = json!({"sessionId":session["id"],"throughSequence":through});
    assert_eq!(
        peer.call("session/context", options.clone()),
        before_migration
    );

    for mut selection in [
        json!({}),
        json!({"selection":"fit_recent","scanLimit":2}),
        json!({"selection":"match_recent","historyQuery":{"query":"needle","role":"user"}}),
        json!({"taskIds":[second["id"]]}),
    ] {
        selection["afterSequence"] = after.clone();
        let mut request = options.clone();
        request["options"] = selection;
        let preview = peer.call("session/context", request);
        assert_eq!(preview["turns"].as_array().unwrap().len(), 1);
        assert_eq!(preview["turns"][0]["taskId"], second["id"]);
        assert_eq!(preview["omittedTurns"], 1);
        assert_eq!(preview["options"]["afterSequence"], after);
    }
    let mut empty_request = options.clone();
    empty_request["options"] = json!({"afterSequence":through});
    let empty = peer.call("session/context", empty_request);
    assert_eq!(empty["turns"], json!([]));
    assert_eq!(empty["omittedTurns"], 2);
    for invalid in [
        json!({"taskIds":[first["id"]],"afterSequence":after}),
        json!({"afterSequence":through.as_u64().unwrap()+1}),
        json!({"mode":"isolated","afterSequence":after}),
    ] {
        let mut request = options.clone();
        request["options"] = invalid;
        assert_eq!(
            peer.request("session/context", request)["error"]["code"],
            -32602
        );
    }
    let mut zero = options.clone();
    zero["options"] = json!({"afterSequence":0});
    assert_eq!(peer.call("session/context", zero), before_migration);

    let mut pinned = options;
    pinned["options"] = json!({"afterSequence":after});
    let snapshot = peer.call("session/context", pinned.clone());
    let request = json!({"sessionId":session["id"],"input":"Continue the handoff", "clientRequestId":"follow", "context":{"afterSequence":after}});
    let receipt = peer.call("task/startSummary", request.clone());
    let task_id = receipt["id"].as_str().unwrap();
    let waiting = peer.wait_for_status(task_id, "waiting_approval");
    assert_eq!(waiting["context"], snapshot);
    let third = approve_report(&mut peer, &session, "a later report", "third");
    drop(peer);

    let mut peer = Peer::start(&db);
    assert_eq!(peer.call("session/context", pinned), snapshot);
    let live = peer.call(
        "session/context",
        json!({"sessionId":session["id"],"options":{"afterSequence":after}}),
    );
    assert_eq!(live["turns"].as_array().unwrap().len(), 2);
    assert_eq!(live["turns"][1]["taskId"], third["id"]);
    assert_eq!(
        peer.call("task/startSummary", request.clone())["id"],
        task_id
    );
    let replayed = peer.call("task/start", request.clone());
    assert_eq!(replayed["id"], task_id);
    assert_eq!(replayed["context"], snapshot);
    let mut changed = request;
    changed["context"]["afterSequence"] = json!(0);
    assert_eq!(
        peer.request("task/startSummary", changed)["error"]["code"],
        -32009
    );
    drop(peer);
    let root = root.canonicalize().unwrap();
    assert_eq!(root.parent(), Some(temp_dir.as_path()));
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn task_creation_reuses_the_preview_window_after_other_tasks_complete() {
    let temp_dir = std::env::temp_dir().canonicalize().unwrap();
    let root = temp_dir.join(format!("fliuno-context-pin-{}", uuid::Uuid::new_v4()));
    fs::create_dir(&root).unwrap();
    let db = root.join("kernel.db");
    let mut peer = Peer::start(&db);
    let workspace = peer.call("workspace/open", json!({"root":root}));
    let session = peer.call("session/create", json!({"workspaceId":workspace["id"]}));
    approve_report(&mut peer, &session, "before the handoff", "before");
    let initial = peer.call("session/context", json!({"sessionId":session["id"]}));
    let after = initial["throughSequence"].clone();
    let selected = approve_report(&mut peer, &session, "after the handoff", "selected");
    let through = peer.call("events/getWatermark", json!({}))["throughSequence"].clone();
    drop(peer);

    // Legacy snapshots and request receipts have no pinned upper bound.
    rusqlite::Connection::open(&db)
        .unwrap()
        .pragma_update(None, "user_version", 49)
        .unwrap();
    let mut peer = Peer::start(&db);
    let old_request =
        json!({"sessionId":session["id"],"input":"after the handoff","clientRequestId":"selected"});
    assert_eq!(
        peer.call("task/startSummary", old_request)["id"],
        selected["id"]
    );

    let context = json!({"detail":"conversation","afterSequence":after,"throughSequence":through});
    let preview_request = json!({"sessionId":session["id"], "options":context});
    let preview = peer.call("session/context", preview_request.clone());
    assert_eq!(preview["turns"].as_array().unwrap().len(), 1);
    assert_eq!(preview["turns"][0]["taskId"], selected["id"]);
    assert_eq!(preview["throughSequence"], through);
    assert_eq!(preview["options"]["throughSequence"], through);
    let mut both = preview_request.clone();
    both["throughSequence"] = through.clone();
    assert_eq!(peer.call("session/context", both.clone()), preview);
    both["throughSequence"] = after.clone();
    assert_eq!(
        peer.request("session/context", both)["error"]["code"],
        -32602
    );

    let later = approve_report(
        &mut peer,
        &session,
        "completed while awaiting confirmation",
        "later",
    );
    let latest = peer.call("session/context", json!({"sessionId":session["id"],"options":{"afterSequence":after,"detail":"conversation"}}));
    assert_eq!(latest["turns"].as_array().unwrap().len(), 2);
    assert_eq!(latest["turns"][1]["taskId"], later["id"]);

    let request = json!({"sessionId":session["id"],"input":"use the reviewed context","clientRequestId":"pinned","context":context});
    let latest_sequence = latest["throughSequence"].as_u64().unwrap();
    let before_invalid = peer.call("events/getWatermark", json!({}));
    for options in [
        json!({"throughSequence":latest_sequence+1}),
        json!({"afterSequence":after,"throughSequence":0}),
        json!({"mode":"isolated","throughSequence":0}),
        json!({"taskIds":[later["id"]],"throughSequence":through}),
    ] {
        let mut invalid = request.clone();
        invalid["context"] = options;
        assert_eq!(
            peer.request("task/startSummary", invalid)["error"]["code"],
            -32602
        );
    }
    assert_eq!(peer.call("events/getWatermark", json!({})), before_invalid);
    let task = peer.call("task/start", request.clone());
    assert_eq!(task["context"], preview);
    peer.wait_for_status(task["id"].as_str().unwrap(), "waiting_approval");
    let empty = peer.call("task/start", json!({"sessionId":session["id"],"input":"no history at sequence zero","clientRequestId":"at-zero","context":{"throughSequence":0}}));
    assert_eq!(empty["context"]["throughSequence"], 0);
    assert_eq!(empty["context"]["options"]["throughSequence"], 0);
    assert_eq!(empty["context"]["turns"], json!([]));
    assert_eq!(empty["context"]["omittedTurns"], 0);
    peer.wait_for_status(empty["id"].as_str().unwrap(), "waiting_approval");
    drop(peer);

    let mut peer = Peer::start(&db);
    assert_eq!(peer.call("session/context", preview_request), preview);
    assert_eq!(
        peer.call("task/startSummary", request.clone())["id"],
        task["id"]
    );
    assert_eq!(peer.call("task/start", request.clone())["context"], preview);
    let mut changed = request;
    changed["context"]
        .as_object_mut()
        .unwrap()
        .remove("throughSequence");
    assert_eq!(
        peer.request("task/startSummary", changed)["error"]["code"],
        -32009
    );
    drop(peer);
    let root = root.canonicalize().unwrap();
    assert_eq!(root.parent(), Some(temp_dir.as_path()));
    fs::remove_dir_all(root).unwrap();
}
