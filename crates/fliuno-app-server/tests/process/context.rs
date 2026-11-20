//! Exercise frozen context through the executable, including reopen and errors.
use super::*;

#[path = "context/history_list.rs"]
mod history_list;
#[path = "context/history_search.rs"]
mod history_search;

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
    let narrow = peer.call(
        "session/context",
        json!({"sessionId":session["id"],"options":{"selection":"fit_recent","maxBytes":2048,"scanLimit":1}}),
    );
    assert_eq!(narrow["turns"], json!([]));
    assert_eq!(narrow["omittedTurns"], 2);
    for invalid in [
        json!({"selection":"fit_recent","scanLimit":0}),
        json!({"selection":"fit_recent","scanLimit":513}),
        json!({"selection":"fit_recent","mode":"isolated","scanLimit":128}),
        json!({"scanLimit":128}),
        json!({"taskIds":[small["id"]],"scanLimit":128}),
    ] {
        assert_eq!(
            peer.request(
                "session/context",
                json!({"sessionId":session["id"],"options":invalid})
            )["error"]["code"],
            -32602,
        );
    }
    let request = json!({"sessionId":session["id"],"input":"follow up","clientRequestId":"fit","context":opts});
    let follow = peer.call("task/start", request.clone());
    assert_eq!(follow["context"], preview);
    peer.wait_for_status(follow["id"].as_str().unwrap(), "waiting_approval");
    approve_report(&mut peer, &session, "latest small report", "latest");
    drop(peer);
    // Reopen a v46 database with only legacy-shaped context options.
    rusqlite::Connection::open(&db)
        .unwrap()
        .pragma_update(None, "user_version", 46)
        .unwrap();
    let mut peer = Peer::start(&db);
    let mut explicit_default = request.clone();
    explicit_default["context"]["scanLimit"] = json!(128);
    let retry = peer.call("task/start", explicit_default.clone());
    assert_eq!(retry["id"], follow["id"]);
    assert_eq!(retry["context"], preview);
    let receipt = peer.call("task/startSummary", explicit_default);
    assert_eq!(receipt["id"], follow["id"]);
    assert_eq!(
        rusqlite::Connection::open(&db)
            .unwrap()
            .pragma_query_value(None, "user_version", |row| row.get::<_, u32>(0))
            .unwrap(),
        48
    );
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
fn match_recent_freezes_only_matching_history_with_a_role_and_byte_budget() {
    let root = std::env::temp_dir().join(format!("fliuno-match-context-{}", uuid::Uuid::new_v4()));
    fs::create_dir(&root).unwrap();
    let db = root.join("kernel.db");
    let mut peer = Peer::start(&db);
    let ws = peer.call("workspace/open", json!({"root":root}));
    let session = peer.call("session/create", json!({"workspaceId":ws["id"]}));
    let small = approve_report(&mut peer, &session, "Orion report", "matching");
    approve_report(&mut peer, &session, "unrelated report", "unrelated");
    approve_report(
        &mut peer,
        &session,
        &format!("Orion {}", "long ".repeat(900)),
        "oversized",
    );
    let other = peer.call("session/create", json!({"workspaceId":ws["id"]}));
    approve_report(&mut peer, &other, "Orion other session", "outside");

    let options = json!({"selection":"match_recent","historyQuery":{"query":"  ORION ","role":"user"},"maxBytes":2048,"scanLimit":3});
    let preview = peer.call(
        "session/context",
        json!({"sessionId":session["id"],"options":options}),
    );
    assert_eq!(preview["options"]["historyQuery"]["query"], "ORION");
    assert_eq!(preview["options"]["scanLimit"], 3);
    assert_eq!(preview["turns"].as_array().unwrap().len(), 1);
    assert_eq!(preview["turns"][0]["taskId"], small["id"]);
    assert!(
        preview["turns"][0]["messages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|message| message["role"] == "tool_result")
    );
    assert_eq!(preview["omittedTurns"], 2);
    assert!(serde_json::to_vec(&preview).unwrap().len() <= 2048);
    let mut narrow = options.clone();
    narrow["scanLimit"] = json!(2);
    assert_eq!(
        peer.call(
            "session/context",
            json!({"sessionId":session["id"],"options":narrow})
        )["turns"],
        json!([])
    );

    let mut assistant = options.clone();
    assistant["historyQuery"]["role"] = json!("assistant");
    assert_eq!(
        peer.call(
            "session/context",
            json!({"sessionId":session["id"],"options":assistant})
        )["turns"],
        json!([])
    );
    let mut compact = options.clone();
    compact["detail"] = json!("conversation");
    let compact_preview = peer.call(
        "session/context",
        json!({"sessionId":session["id"],"options":compact}),
    );
    assert_eq!(compact_preview["turns"][0]["taskId"], small["id"]);
    assert_eq!(
        compact_preview["turns"][0]["messages"]
            .as_array()
            .unwrap()
            .len(),
        2
    );

    let mut request = json!({"sessionId":session["id"],"input":"follow up","clientRequestId":"match","context":options});
    let follow = peer.call("task/start", request.clone());
    assert_eq!(follow["context"], preview);
    peer.wait_for_status(follow["id"].as_str().unwrap(), "waiting_approval");
    let later = approve_report(&mut peer, &session, "Orion later", "later");
    let current = peer.call(
        "session/context",
        json!({"sessionId":session["id"],"options":options}),
    );
    assert!(
        current["turns"]
            .as_array()
            .unwrap()
            .iter()
            .any(|turn| turn["taskId"] == later["id"])
    );
    drop(peer);

    let mut peer = Peer::start(&db);
    request["context"]["historyQuery"]["query"] = json!("ORION");
    let retry = peer.call("task/start", request.clone());
    assert_eq!(retry["id"], follow["id"]);
    assert_eq!(retry["context"], preview);
    let mut changed_limit = request.clone();
    changed_limit["context"]["scanLimit"] = json!(2);
    for method in ["task/start", "task/startSummary"] {
        assert_eq!(
            peer.request(method, changed_limit.clone())["error"]["code"],
            -32009
        );
    }
    request["context"]["historyQuery"]["query"] = json!("other topic");
    assert_eq!(peer.request("task/start", request)["error"]["code"], -32009);
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
    frozen["role"] = json!("user");
    assert_eq!(peer.call("session/searchHistory", frozen.clone()), first);
    let mut assistant = frozen.clone();
    assistant["role"] = json!("assistant");
    assert_eq!(
        peer.call("session/searchHistory", assistant.clone())["matches"],
        json!([])
    );
    assistant["query"] = json!("ARTIFACT CREATED");
    assert_eq!(peer.call("session/searchHistory", assistant.clone()), first);
    assistant["role"] = json!("user");
    assert_eq!(
        peer.call("session/searchHistory", assistant.clone())["matches"],
        json!([])
    );
    assistant["role"] = json!("tool");
    assert_eq!(
        peer.request("session/searchHistory", assistant)["error"]["code"],
        -32602
    );
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

#[test]
fn workspace_history_search_backfills_and_pages_across_sessions_without_crossing_workspaces() {
    let root =
        std::env::temp_dir().join(format!("fliuno-workspace-search-{}", uuid::Uuid::new_v4()));
    let other_root = root.join("other");
    fs::create_dir_all(&other_root).unwrap();
    let db = root.join("kernel.db");
    let method = "workspace/searchHistory";
    let mut peer = Peer::start(&db);
    let workspace = peer.call("workspace/open", json!({"root":root}));
    let other_ws = peer.call("workspace/open", json!({"root":other_root}));
    let first_session = peer.call("session/create", json!({"workspaceId":workspace["id"]}));
    let second_session = peer.call("session/create", json!({"workspaceId":workspace["id"]}));
    let foreign_session = peer.call("session/create", json!({"workspaceId":other_ws["id"]}));
    let first = approve_report(&mut peer, &first_session, "case A", "a");
    let second = approve_report(&mut peer, &second_session, "case B", "b");
    approve_report(
        &mut peer,
        &foreign_session,
        "case other workspace",
        "foreign",
    );
    peer.call(
        "session/setArchived",
        json!({"sessionId":second_session["id"],"archived":true}),
    );
    drop(peer);

    rusqlite::Connection::open(&db).unwrap().execute_batch(
        "DROP TRIGGER workspace_turn_index_insert; DROP TABLE workspace_turn_index; PRAGMA user_version=32;",
    ).unwrap();
    let mut peer = Peer::start(&db);
    let request = json!({"workspaceId":workspace["id"],"query":" CASE ","role":"user","limit":1});
    for (method, scope) in [
        (
            "session/searchHistory",
            json!({"sessionId":first_session["id"]}),
        ),
        (
            "workspace/searchHistory",
            json!({"workspaceId":workspace["id"]}),
        ),
    ] {
        for invalid in [0, 513] {
            let mut params = scope.clone();
            params["query"] = json!("case");
            params["scanLimit"] = json!(invalid);
            assert_eq!(peer.request(method, params)["error"]["code"], -32602);
        }
    }
    let page = peer.call(method, request.clone());
    assert_eq!(page["scannedTurns"], 1);
    assert_eq!(page["matches"][0]["taskId"], second["id"]);
    assert_eq!(page["matches"][0]["sessionId"], second_session["id"]);
    let late = approve_report(&mut peer, &first_session, "case later", "late");
    drop(peer);

    let mut peer = Peer::start(&db);
    let mut next = request.clone();
    next["throughSequence"] = page["throughSequence"].clone();
    assert_eq!(peer.call(method, next.clone()), page);
    next["beforeSequence"] = page["nextBeforeSequence"].clone();
    let second_page = peer.call(method, next);
    assert_eq!(second_page["matches"].as_array().unwrap().len(), 1);
    assert_eq!(second_page["matches"][0]["taskId"], first["id"]);
    assert_eq!(second_page["matches"][0]["sessionId"], first_session["id"]);
    assert!(second_page["nextBeforeSequence"].is_null());
    assert_eq!(
        peer.call(method, request.clone())["matches"][0]["taskId"],
        late["id"]
    );
    let mut invalid = request;
    invalid["workspaceId"] = json!("missing-workspace");
    assert_eq!(peer.request(method, invalid)["error"]["code"], -32004);
    drop(peer);
    fs::remove_dir_all(root).unwrap();
}
