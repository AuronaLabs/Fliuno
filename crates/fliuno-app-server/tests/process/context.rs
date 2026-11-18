//! Exercise frozen context through the executable, including reopen and errors.
use super::*;

fn approve_report(peer: &mut Peer, session: &Value, input: &str, key: &str) -> Value {
    let task = peer.call(
        "task/start",
        json!({"sessionId":session["id"],"input":input,"clientRequestId":key}),
    );
    let waiting = peer.wait_for_status(task["id"].as_str().unwrap(), "waiting_approval");
    peer.call("approval/respond", json!({"taskId":task["id"],"approvalId":waiting["pendingApproval"]["id"],"decision":"allow_once"}));
    peer.wait_for_status(task["id"].as_str().unwrap(), "completed")
}

#[test]
fn context_preview_snapshot_and_idempotency_survive_restart() {
    let root = std::env::temp_dir().join(format!(
        "fliuno-context-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir(&root).unwrap();
    let db = root.join("kernel.db");
    let mut peer = Peer::start(&db);
    let ws = peer.call("workspace/open", json!({"root":root}));
    let session = peer.call("session/create", json!({"workspaceId":ws["id"]}));
    let first = approve_report(&mut peer, &session, "第一份报告", "first");
    let artifacts = peer.call("artifacts/list", json!({"taskId":first["id"]}));
    let preview = peer.call("session/context", json!({"sessionId":session["id"]}));
    assert_eq!(preview["turns"][0]["taskId"], first["id"]);
    assert_eq!(
        preview["turns"][0]["artifacts"][0]["id"],
        artifacts[0]["id"]
    );
    let follow = peer.call(
        "task/start",
        json!({"sessionId":session["id"],"input":"follow up","clientRequestId":"follow"}),
    );
    assert_eq!(follow["context"], preview);
    peer.wait_for_status(follow["id"].as_str().unwrap(), "waiting_approval");
    approve_report(&mut peer, &session, "后来完成的任务", "later");
    assert_eq!(
        peer.call("task/get", json!({"taskId":follow["id"]}))["context"],
        preview
    );
    let latest = peer.call(
        "session/context",
        json!({"sessionId":session["id"],"options":{"maxTurns":1}}),
    );
    assert_eq!(latest["turns"].as_array().unwrap().len(), 1);
    assert_eq!(latest["omittedTurns"], 1);
    assert_eq!(
        peer.request(
            "session/context",
            json!({"sessionId":session["id"],"options":{"maxBytes":1023}})
        )["error"]["code"],
        -32602
    );
    assert_eq!(
        peer.request("session/context", json!({"sessionId":"missing"}))["error"]["code"],
        -32004
    );
    drop(peer);

    let mut peer = Peer::start(&db);
    let retry = peer.call("task/start", json!({"sessionId":session["id"],"input":"follow up","clientRequestId":"follow","context":{}}));
    assert_eq!(retry["id"], follow["id"]);
    assert_eq!(retry["context"], preview);
    assert_eq!(
        retry["pendingApproval"]["id"],
        peer.call("task/get", json!({"taskId":follow["id"]}))["pendingApproval"]["id"]
    );
    assert_eq!(peer.request("task/start", json!({"sessionId":session["id"],"input":"follow up","clientRequestId":"follow","context":{"mode":"isolated"}}))["error"]["code"], -32009);
    let next = peer.call(
        "task/start",
        json!({"sessionId":session["id"],"input":"after restart"}),
    );
    assert_eq!(next["context"]["turns"].as_array().unwrap().len(), 2);
    assert_eq!(next["context"]["turns"][0]["taskId"], first["id"]);
    let isolated = peer.call(
        "task/start",
        json!({"sessionId":session["id"],"input":"private","context":{"mode":"isolated"}}),
    );
    assert!(isolated["context"]["turns"].as_array().unwrap().is_empty());
    assert_eq!(isolated["context"]["options"]["mode"], "isolated");
    let other = peer.call("session/create", json!({"workspaceId":ws["id"]}));
    assert!(
        peer.call("session/context", json!({"sessionId":other["id"]}))["turns"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    drop(peer);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn fit_recent_skips_large_turns_and_keeps_the_selection_across_restart() {
    let root = std::env::temp_dir().join(format!(
        "fliuno-fit-context-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir(&root).unwrap();
    let db = root.join("kernel.db");
    let mut peer = Peer::start(&db);
    let ws = peer.call("workspace/open", json!({"root":root}));
    let session = peer.call("session/create", json!({"workspaceId":ws["id"]}));
    let small = approve_report(&mut peer, &session, "small report", "small");
    approve_report(&mut peer, &session, &"超大".repeat(2048), "large");
    let contiguous = peer.call(
        "session/context",
        json!({"sessionId":session["id"],"options":{"maxBytes":2048}}),
    );
    assert_eq!(contiguous["turns"], json!([]));
    let opts = json!({"selection":"fit_recent","maxBytes":2048});
    let preview = peer.call(
        "session/context",
        json!({"sessionId":session["id"], "options":opts}),
    );
    assert_eq!(preview["turns"][0]["taskId"], small["id"]);
    assert_eq!(preview["omittedTurns"], 1);
    let request = json!({"sessionId":session["id"],"input":"follow up","clientRequestId":"fit","context":opts});
    let follow = peer.call("task/start", request.clone());
    assert_eq!(follow["context"], preview);
    peer.wait_for_status(follow["id"].as_str().unwrap(), "waiting_approval");
    approve_report(&mut peer, &session, "latest small report", "latest");
    drop(peer);
    let mut peer = Peer::start(&db);
    let retry = peer.call("task/start", request.clone());
    assert_eq!(retry["id"], follow["id"]);
    assert_eq!(retry["context"], preview);
    let mut different = request;
    different["context"]["selection"] = json!("recent");
    assert_eq!(
        peer.request("task/start", different)["error"]["code"],
        -32009
    );
    drop(peer);
    for entry in fs::read_dir(&root).unwrap() {
        fs::remove_file(entry.unwrap().path()).unwrap();
    }
    fs::remove_dir(root).unwrap();
}
