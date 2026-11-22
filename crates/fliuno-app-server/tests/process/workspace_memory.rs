use super::*;
#[path = "workspace_memory_vectors.rs"]
mod vectors;

#[test]
fn workspace_memory_is_scoped_versioned_audited_and_persistent() {
    let root = std::env::temp_dir().join(format!(
        "fliuno-workspace-memory-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let first_root = root.join("first");
    let second_root = root.join("second");
    fs::create_dir_all(&first_root).unwrap();
    fs::create_dir_all(&second_root).unwrap();
    let db = root.join("kernel.db");
    let mut peer = Peer::start(&db);
    let first = peer.call("workspace/open", json!({"root":first_root}));
    let second = peer.call("workspace/open", json!({"root":second_root}));
    let put = json!({
        "workspaceId":first["id"],"key":"build-guidance",
        "content":"Run cargo test -p fliuno-core before merging."
    });
    let created = peer.call("workspace/memory/put", put.clone());
    assert_eq!(created["revision"], 1);
    assert!(created["deletedAtMs"].is_null());
    assert_eq!(peer.call("workspace/memory/put", put.clone()), created);

    let stale = peer.request(
        "workspace/memory/put",
        json!({"workspaceId":first["id"],"key":"build-guidance",
            "content":"Use another command.","expectedRevision":0}),
    );
    assert_eq!(stale["error"]["code"], -32009);
    let updated = peer.call(
        "workspace/memory/put",
        json!({"workspaceId":first["id"],"key":"build-guidance",
            "content":"Run cargo test --workspace before merging.","expectedRevision":1}),
    );
    assert_eq!(updated["revision"], 2);

    let other = peer.call(
        "workspace/memory/put",
        json!({"workspaceId":second["id"],"key":"build-guidance",
            "content":"This belongs to the other project."}),
    );
    assert_ne!(created["id"], other["id"]);
    let first_search = peer.call(
        "workspace/memory/search",
        json!({"workspaceId":first["id"],"query":"workspace","limit":10}),
    );
    assert_eq!(first_search["items"].as_array().unwrap().len(), 1);
    let second_search = peer.call(
        "workspace/memory/search",
        json!({"workspaceId":second["id"],"query":"project","limit":10}),
    );
    assert_eq!(second_search["items"].as_array().unwrap().len(), 1);
    assert_eq!(
        peer.call(
            "workspace/memory/get",
            json!({"workspaceId":first["id"],"key":"build-guidance"}),
        )["revision"],
        2
    );

    let deleted = peer.call(
        "workspace/memory/delete",
        json!({"workspaceId":first["id"],"key":"build-guidance","expectedRevision":2}),
    );
    assert_eq!(deleted["revision"], 3);
    assert!(deleted["deletedAtMs"].as_u64().is_some());
    assert!(
        peer.call(
            "workspace/memory/search",
            json!({"workspaceId":first["id"],"query":"workspace"}),
        )["items"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        peer.request(
            "workspace/memory/get",
            json!({"workspaceId":first["id"],"key":"build-guidance"}),
        )["error"]["code"],
        -32004
    );

    let restored = peer.call(
        "workspace/memory/put",
        json!({"workspaceId":first["id"],"key":"build-guidance",
            "content":"Run cargo test --workspace before merging.","expectedRevision":3}),
    );
    assert_eq!(restored["revision"], 4);
    assert!(restored["deletedAtMs"].is_null());
    let audit = peer.call(
        "workspace/memory/listAudit",
        json!({"workspaceId":first["id"],"limit":100}),
    );
    let actions: Vec<_> = audit["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["action"].as_str().unwrap())
        .collect();
    assert_eq!(
        actions,
        [
            "created", "updated", "searched", "read", "deleted", "searched", "restored"
        ]
    );
    assert!(audit["items"].as_array().unwrap().iter().any(|entry| {
        entry["contentSha256"]
            .as_str()
            .is_some_and(|hash| hash.len() == 64)
            && entry.get("content").is_none()
    }));
    drop(peer);

    let mut peer = Peer::start(&db);
    assert_eq!(
        peer.call(
            "workspace/memory/get",
            json!({"workspaceId":first["id"],"key":"build-guidance"}),
        )["revision"],
        4
    );
    let with_deleted = peer.call(
        "workspace/memory/list",
        json!({"workspaceId":first["id"],"includeDeleted":true,"limit":100}),
    );
    assert_eq!(with_deleted["items"].as_array().unwrap().len(), 1);
    drop(peer);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn task_context_freezes_bounded_workspace_memory_across_retry_restart_and_delete() {
    let root = std::env::temp_dir().join(format!(
        "fliuno-memory-context-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let first_root = root.join("first");
    let second_root = root.join("second");
    fs::create_dir_all(&first_root).unwrap();
    fs::create_dir_all(&second_root).unwrap();
    let db = root.join("kernel.db");
    let mut peer = Peer::start(&db);
    let first = peer.call("workspace/open", json!({"root":first_root}));
    let second = peer.call("workspace/open", json!({"root":second_root}));
    let session = peer.call("session/create", json!({"workspaceId":first["id"]}));
    let relevant = peer.call(
        "workspace/memory/put",
        json!({"workspaceId":first["id"],"key":"build-guidance",
            "content":"Run cargo test -p fliuno-core before merging."}),
    );
    peer.call(
        "workspace/memory/put",
        json!({"workspaceId":first["id"],"key":"kitchen-note",
            "content":"Use cinnamon in the bread recipe."}),
    );
    peer.call(
        "workspace/memory/put",
        json!({"workspaceId":second["id"],"key":"private-build-guidance",
            "content":"Run cargo test -p fliuno-core with PRIVATE_WORKSPACE_SECRET."}),
    );

    let request = json!({"sessionId":session["id"],"input":"Run cargo test for fliuno-core now",
        "clientRequestId":"memory-context"});
    let task = peer.call("task/start", request.clone());
    let frozen = task["context"].clone();
    assert_eq!(frozen["memories"].as_array().unwrap().len(), 1);
    assert_eq!(frozen["memories"][0]["id"], relevant["id"]);
    assert_eq!(frozen["memories"][0]["revision"], 1);
    assert!(!frozen.to_string().contains("PRIVATE_WORKSPACE_SECRET"));
    assert!(!frozen.to_string().contains("cinnamon"));

    let audit = peer.call(
        "workspace/memory/listAudit",
        json!({"workspaceId":first["id"],"limit":100}),
    );
    assert!(audit["items"].as_array().unwrap().iter().any(|entry| {
        entry["action"] == "context_injected"
            && entry["actorTaskId"] == task["id"]
            && entry["memoryId"] == relevant["id"]
    }));

    peer.call(
        "workspace/memory/put",
        json!({"workspaceId":first["id"],"key":"build-guidance",
            "content":"Changed after the task was created.","expectedRevision":1}),
    );
    let retry = peer.call("task/start", request);
    assert_eq!(retry["id"], task["id"]);
    assert_eq!(retry["context"], frozen);

    let isolated = peer.call(
        "task/start",
        json!({"sessionId":session["id"],"input":"Run cargo test for fliuno-core now",
            "context":{"mode":"isolated"}}),
    );
    assert!(isolated["context"].get("memories").is_none());
    drop(peer);

    let mut peer = Peer::start(&db);
    let restored = peer.call("task/get", json!({"taskId":task["id"]}));
    assert_eq!(restored["context"], frozen);
    peer.call(
        "workspace/memory/delete",
        json!({"workspaceId":first["id"],"key":"build-guidance","expectedRevision":2}),
    );
    let after_delete = peer.call(
        "task/start",
        json!({"sessionId":session["id"],"input":"Run cargo test for fliuno-core again"}),
    );
    assert!(after_delete["context"].get("memories").is_none());
    drop(peer);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn task_context_memory_matches_cjk_and_skips_unmatched_or_over_budget_records() {
    let root = std::env::temp_dir().join(format!(
        "fliuno-memory-context-bounds-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(&root).unwrap();
    let workspace_root = root.join("project");
    fs::create_dir(&workspace_root).unwrap();
    let db = root.join("kernel.db");
    let mut peer = Peer::start(&db);
    let workspace = peer.call("workspace/open", json!({"root":workspace_root}));
    let session = peer.call("session/create", json!({"workspaceId":workspace["id"]}));
    let small = peer.call(
        "workspace/memory/put",
        json!({"workspaceId":workspace["id"],"key":"a-small-build-note",
            "content":"项目构建流程需要检查。"}),
    );
    let chinese = peer.call(
        "task/start",
        json!({"sessionId":session["id"],"input":"项目构建流程检查"}),
    );
    assert_eq!(chinese["context"]["memories"][0]["id"], small["id"]);

    let unmatched = peer.call(
        "task/start",
        json!({"sessionId":session["id"],"input":"天气预报怎么样"}),
    );
    assert!(unmatched["context"].get("memories").is_none());

    let large = peer.call(
        "workspace/memory/put",
        json!({"workspaceId":workspace["id"],"key":"z-large-build-note",
            "content":"项目构建流程需要检查。".repeat(180)}),
    );
    let budgeted = peer.call(
        "task/start",
        json!({"sessionId":session["id"],"input":"项目构建流程检查",
            "context":{"maxBytes":1024}}),
    );
    let memories = budgeted["context"]["memories"].as_array().unwrap();
    assert_eq!(memories.len(), 1);
    assert_eq!(memories[0]["id"], small["id"]);
    assert_ne!(memories[0]["id"], large["id"]);

    let audit = peer.call(
        "workspace/memory/listAudit",
        json!({"workspaceId":workspace["id"],"limit":100}),
    );
    assert!(audit["items"].as_array().unwrap().iter().any(|entry| {
        entry["action"] == "context_searched" && entry["actorTaskId"] == unmatched["id"]
    }));
    drop(peer);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn workspace_memory_schema_migrates_from_v54() {
    let root = std::env::temp_dir().join(format!(
        "fliuno-workspace-memory-migrate-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(&root).unwrap();
    let workspace_root = root.join("project");
    fs::create_dir(&workspace_root).unwrap();
    let db = root.join("kernel.db");
    let mut peer = Peer::start(&db);
    let workspace = peer.call("workspace/open", json!({"root":workspace_root}));
    drop(peer);

    rusqlite::Connection::open(&db)
        .unwrap()
        .execute_batch(
            "DROP TABLE workspace_memory_audit;
             DROP TABLE workspace_memory_records;
             PRAGMA user_version=54;",
        )
        .unwrap();

    let mut peer = Peer::start(&db);
    let memory = peer.call(
        "workspace/memory/put",
        json!({"workspaceId":workspace["id"],"key":"migration","content":"v54 upgrade"}),
    );
    assert_eq!(memory["revision"], 1);
    let version: u32 = rusqlite::Connection::open(&db)
        .unwrap()
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .unwrap();
    assert_eq!(version, 68);
    drop(peer);
    fs::remove_dir_all(root).unwrap();
}
