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
    let later = approve_report(&mut peer, &session, "后来完成的任务", "later");
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
    let history = json!({"sessionId":session["id"],"throughSequence":preview["throughSequence"]});
    assert_eq!(peer.call("session/context", history.clone()), preview);
    let mut selected = history;
    selected["options"] = json!({"taskIds":[later["id"]]});
    assert_eq!(
        peer.request("session/context", selected)["error"]["code"],
        -32602
    );
    let empty = peer.call(
        "session/context",
        json!({"sessionId":session["id"],"throughSequence":0}),
    );
    assert_eq!(empty["throughSequence"], 0);
    assert_eq!(empty["omittedTurns"], 0);
    assert_eq!(empty["turns"], json!([]));
    assert_eq!(
        peer.request(
            "session/context",
            json!({"sessionId":session["id"],"throughSequence":u64::MAX})
        )["error"]["code"],
        -32602
    );
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

#[test]
fn conversation_history_fits_larger_tool_turns_and_backfills_without_rewriting_tasks() {
    let root = std::env::temp_dir()
        .canonicalize()
        .unwrap()
        .join(format!("fliuno-conversation-{}", uuid::Uuid::new_v4()));
    fs::create_dir(&root).unwrap();
    let db = root.join("kernel.db");
    let mut peer = Peer::start(&db);
    let ws = peer.call("workspace/open", json!({"root":root}));
    let session = peer.call("session/create", json!({"workspaceId":ws["id"]}));
    let input = "界".repeat(600);
    let done = approve_report(&mut peer, &session, &input, "source");
    let full = peer.call("session/context", json!({"sessionId":session["id"]}));
    assert!(
        full["turns"][0]["messages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|message| message["role"] == "tool_result")
    );
    assert!(full["options"].get("detail").is_none());

    let narrow = peer.call(
        "session/context",
        json!({"sessionId":session["id"],"options":{"maxBytes":3000}}),
    );
    assert_eq!(narrow["omittedTurns"], 1);
    assert_eq!(narrow["turns"], json!([]));
    let params =
        json!({"sessionId":session["id"],"options":{"detail":"conversation","maxBytes":3000}});
    let compact = peer.call("session/context", params.clone());
    let turn = &compact["turns"][0];
    assert_eq!(compact["omittedTurns"], 0);
    assert_eq!(turn["taskId"], done["id"]);
    assert_eq!(turn["artifacts"], full["turns"][0]["artifacts"]);
    assert_eq!(
        turn["messages"],
        json!([
            {"role":"user","text":input},
            {"role":"assistant","text":"Demo artifact created."}
        ])
    );
    let selected = peer.call(
        "session/context",
        json!({"sessionId":session["id"],"options":{
            "detail":"conversation","maxBytes":3000,"taskIds":[done["id"]]
        }}),
    );
    assert_eq!(selected["turns"], compact["turns"]);
    drop(peer);
    rusqlite::Connection::open(&db)
        .unwrap()
        .execute_batch("DROP TABLE conversation_turns; PRAGMA user_version=21;")
        .unwrap();
    let mut peer = Peer::start(&db);
    assert_eq!(peer.call("session/context", params.clone()), compact);
    assert_eq!(peer.call("task/get", json!({"taskId":done["id"]})), done);

    let request = json!({"sessionId":session["id"],"input":"follow up","clientRequestId":"compact","context":params["options"]});
    let follow = peer.call("task/start", request.clone());
    assert_eq!(follow["context"], compact);
    peer.wait_for_status(follow["id"].as_str().unwrap(), "waiting_approval");
    drop(peer);
    let mut peer = Peer::start(&db);
    let retry = peer.call("task/start", request.clone());
    assert_eq!(retry["id"], follow["id"]);
    assert_eq!(retry["context"], compact);
    let mut changed = request;
    changed["context"]["detail"] = json!("full");
    assert_eq!(peer.request("task/start", changed)["error"]["code"], -32009);
    drop(peer);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn history_search_pages_keep_their_watermark_and_can_select_context_after_restart() {
    let root = std::env::temp_dir().join(format!(
        "fliuno-search-context-{}-{}",
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
    let other = peer.call("session/create", json!({"workspaceId":ws["id"]}));
    let older = approve_report(&mut peer, &session, "Audit \"报告\" old", "old");
    let newer = approve_report(&mut peer, &session, "AUDIT \"报告\" new", "new");
    approve_report(&mut peer, &other, "audit \"报告\" foreign", "foreign");
    let request = json!({"sessionId":session["id"],"query":" AuDiT \"报告\" ","limit":1});
    let first = peer.call("session/searchHistory", request.clone());
    assert_eq!(first["scannedTurns"], 1);
    assert_eq!(first["matches"][0]["taskId"], newer["id"]);
    assert_eq!(
        first["nextBeforeSequence"],
        first["matches"][0]["completionSequence"]
    );
    approve_report(&mut peer, &session, "audit \"报告\" later", "later");
    drop(peer);

    let mut peer = Peer::start(&db);
    let mut frozen = request;
    frozen["throughSequence"] = first["throughSequence"].clone();
    assert_eq!(peer.call("session/searchHistory", frozen.clone()), first);
    frozen["beforeSequence"] = first["nextBeforeSequence"].clone();
    let next = peer.call("session/searchHistory", frozen.clone());
    assert_eq!(next["matches"][0]["taskId"], older["id"]);
    assert!(next["nextBeforeSequence"].is_null());
    let selected = peer.call("session/context", json!({"sessionId":session["id"],
        "throughSequence":first["throughSequence"],"options":{"taskIds":[next["matches"][0]["taskId"]]}}));
    assert_eq!(selected["turns"].as_array().unwrap().len(), 1);
    assert_eq!(selected["turns"][0]["taskId"], older["id"]);
    frozen["beforeSequence"] = json!(0);
    assert_eq!(
        peer.request("session/searchHistory", frozen)["error"]["code"],
        -32602
    );
    drop(peer);
    fs::remove_dir_all(root).unwrap();
}
