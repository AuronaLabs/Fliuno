//! Filter a persisted catalog across real task state transitions and restart.
use super::*;

#[test]
fn filtered_task_summaries_keep_the_catalog_cursor_across_migration_and_updates() {
    let root = std::env::temp_dir()
        .canonicalize()
        .unwrap()
        .join(format!("fliuno-task-status-{}", uuid::Uuid::new_v4()));
    fs::create_dir(&root).unwrap();
    let db = root.join("kernel.db");
    let mut peer = Peer::start(&db);
    let workspace = peer.call("workspace/open", json!({"root":root}));
    let session = peer.call("session/create", json!({"workspaceId":workspace["id"]}));
    let sibling = peer.call("session/create", json!({"workspaceId":workspace["id"]}));
    let foreign_root = root.join("foreign");
    fs::create_dir(&foreign_root).unwrap();
    let foreign_workspace = peer.call("workspace/open", json!({"root":foreign_root}));
    let foreign = peer.call(
        "session/create",
        json!({"workspaceId":foreign_workspace["id"]}),
    );
    let mut start = |session: &Value, input: &str| {
        let task = peer.call(
            "task/start",
            json!({"sessionId":session["id"],"input":input}),
        );
        peer.wait_for_status(task["id"].as_str().unwrap(), "waiting_approval")
    };
    let first = start(&session, "first");
    let cancelled = start(&session, "cancelled");
    let third = start(&session, "third");
    let _foreign = start(&foreign, "another workspace");
    let sibling_task = start(&sibling, "same workspace");
    peer.call("task/cancel", json!({"taskId":cancelled["id"]}));
    let page = peer.call(
        "task/listSummaries",
        json!({"sessionId":session["id"],"limit":1}),
    );
    drop(peer);

    rusqlite::Connection::open(&db)
        .unwrap()
        .execute_batch(
            "DROP TRIGGER task_summary_status_insert;
         DROP TRIGGER task_summary_status_update;
         DROP TABLE task_summary_status;
         PRAGMA user_version=22;",
        )
        .unwrap();
    let mut peer = Peer::start(&db);
    let scope = &workspace["id"];
    assert_eq!(
        peer.call(
            "task/listWorkspaceSummaries",
            json!({"workspaceId":scope,"limit":1})
        ),
        page
    );
    let workspace_page = peer.call(
        "task/listWorkspaceSummaries",
        json!({
            "workspaceId":scope,"status":"waiting_approval","after":page["nextAfter"]
        }),
    );
    let ids: Vec<_> = workspace_page["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["id"].clone())
        .collect();
    assert_eq!(ids, [third["id"].clone(), sibling_task["id"].clone()]);
    assert_eq!(workspace_page["hasMore"], false);
    assert_eq!(
        peer.call(
            "task/listSummaries",
            json!({"sessionId":session["id"],"status":"waiting_approval","limit":1})
        ),
        page
    );
    let next = peer.call("task/listSummaries", json!({"sessionId":session["id"],"status":"waiting_approval","after":page["nextAfter"],"limit":1}));
    assert_eq!(next["items"][0]["id"], third["id"]);
    assert_eq!(next["hasMore"], false);
    peer.call(
        "approval/respond",
        json!({"taskId":first["id"],"approvalId":first["pendingApproval"]["id"],"decision":"deny"}),
    );
    let failed = peer.call(
        "task/listSummaries",
        json!({"sessionId":session["id"],"status":"failed"}),
    );
    assert_eq!(failed["items"][0]["id"], first["id"]);
    assert_eq!(failed["nextAfter"], page["nextAfter"]);
    assert_eq!(
        peer.call(
            "task/listSummaries",
            json!({"sessionId":session["id"],"status":"waiting_approval"})
        ),
        next
    );
    assert_eq!(
        peer.request(
            "task/listSummaries",
            json!({"sessionId":session["id"],"status":"unknown"})
        )["error"]["code"],
        -32602
    );
    let workspace_failed = peer.call(
        "task/listWorkspaceSummaries",
        json!({
            "workspaceId":scope,"status":"failed"
        }),
    );
    assert_eq!(workspace_failed, failed);
    let newer = peer.call(
        "task/start",
        json!({"sessionId":sibling["id"],"input":"after migration"}),
    );
    peer.wait_for_status(newer["id"].as_str().unwrap(), "waiting_approval");
    let tail = peer.call(
        "task/listWorkspaceSummaries",
        json!({
            "workspaceId":scope,"after":workspace_page["nextAfter"]
        }),
    );
    assert_eq!(tail["items"].as_array().unwrap().len(), 1);
    assert_eq!(tail["items"][0]["id"], newer["id"]);
    assert_eq!(
        peer.request(
            "task/listWorkspaceSummaries",
            json!({"workspaceId":"absent"})
        )["error"]["code"],
        -32004
    );
    drop(peer);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn full_task_pages_respect_compact_bytes_and_advance_past_an_oversized_task() {
    let root = std::env::temp_dir().join(format!("fliuno-task-pages-{}", uuid::Uuid::new_v4()));
    fs::create_dir(&root).unwrap();
    let db = root.join("kernel.db");
    let mut peer = Peer::start(&db);
    let workspace = peer.call("workspace/open", json!({"root":root}));
    let session = peer.call("session/create", json!({"workspaceId":workspace["id"]}));
    let session_id = &session["id"];
    let input = "报告\t\"".repeat(32);
    for input in [&input, &input, &"很大的快照".repeat(512), &input] {
        let task = peer.call("task/start", json!({"sessionId":session_id,"input":input}));
        peer.wait_for_status(task["id"].as_str().unwrap(), "waiting_approval");
    }
    let full = peer.call("task/list", json!({"sessionId":session_id}));
    let all = full["items"].as_array().unwrap();
    assert_eq!(all.len(), 4);
    let target = serde_json::to_vec(&all[..2]).unwrap().len();
    assert!(target > 1024);
    assert!(serde_json::to_vec(&all[2..3]).unwrap().len() > target);
    let first = peer.call(
        "task/list",
        json!({"sessionId":session_id,"targetBytes":target}),
    );
    assert_eq!(first["items"].as_array().unwrap(), &all[..2]);
    assert_eq!(first["hasMore"], true);
    let smaller = peer.call(
        "task/list",
        json!({"sessionId":session_id,"targetBytes":target-1}),
    );
    assert_eq!(smaller["items"].as_array().unwrap(), &all[..1]);
    let by_count = peer.call(
        "task/list",
        json!({"sessionId":session_id,"limit":1,"targetBytes":target}),
    );
    assert_eq!(by_count, smaller);
    let second = peer.call(
        "task/list",
        json!({"sessionId":session_id,"after":first["nextAfter"],"targetBytes":target}),
    );
    assert_eq!(second["items"].as_array().unwrap(), &all[2..3]);
    assert_eq!(second["hasMore"], true);
    let last = peer.call(
        "task/list",
        json!({"sessionId":session_id,"after":second["nextAfter"],"targetBytes":target}),
    );
    assert_eq!(last["items"].as_array().unwrap(), &all[3..]);
    assert_eq!(last["hasMore"], false);
    let summaries = peer.call(
        "task/listSummaries",
        json!({"sessionId":session_id,"after":second["nextAfter"]}),
    );
    assert_eq!(summaries["items"][0]["id"], all[3]["id"]);
    assert_eq!(summaries["nextAfter"], last["nextAfter"]);
    let invalid = peer.request(
        "task/list",
        json!({"sessionId":session_id,"targetBytes":1023}),
    );
    assert_eq!(invalid["error"]["code"], -32602);
    drop(peer);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn task_summary_pages_respect_byte_targets_and_keep_session_workspace_cursors() {
    let root = std::env::temp_dir().join(format!("fliuno-summary-pages-{}", uuid::Uuid::new_v4()));
    fs::create_dir(&root).unwrap();
    let db = root.join("kernel.db");
    let mut peer = Peer::start(&db);
    let workspace = peer.call("workspace/open", json!({"root":root}));
    let session = peer.call("session/create", json!({"workspaceId":workspace["id"]}));
    let mut ids = Vec::new();
    for index in 0..12 {
        let task = peer.call(
            "task/start",
            json!({"sessionId":session["id"],"input":format!("{index} {}", "报告 ".repeat(120))}),
        );
        let task = peer.wait_for_status(task["id"].as_str().unwrap(), "waiting_approval");
        ids.push(task["id"].clone());
    }
    for index in [2, 7] {
        peer.call("task/cancel", json!({"taskId":ids[index]}));
    }
    let full = peer.call(
        "task/listSummaries",
        json!({"sessionId":session["id"],"status":"waiting_approval"}),
    );
    let expected = full["items"].as_array().unwrap();
    assert_eq!(expected.len(), 10);
    let count = (1..expected.len())
        .find(|count| serde_json::to_vec(&expected[..*count]).unwrap().len() >= 1024)
        .unwrap();
    let target = serde_json::to_vec(&expected[..count]).unwrap().len() as u32;

    let first = peer.call(
        "task/listSummaries",
        json!({"sessionId":session["id"],"status":"waiting_approval","targetBytes":target}),
    );
    assert_eq!(first["items"].as_array().unwrap(), &expected[..count]);
    assert_eq!(first["hasMore"], true);
    let workspace_first = peer.call(
        "task/listWorkspaceSummaries",
        json!({"workspaceId":workspace["id"],"status":"waiting_approval","targetBytes":target}),
    );
    assert_eq!(workspace_first, first);
    drop(peer);

    let mut peer = Peer::start(&db);
    let second = peer.call(
        "task/listSummaries",
        json!({"sessionId":session["id"],"status":"waiting_approval","after":first["nextAfter"],"targetBytes":target}),
    );
    assert_eq!(
        second["items"].as_array().unwrap(),
        &expected[count..count * 2]
    );
    let workspace_second = peer.call(
        "task/listWorkspaceSummaries",
        json!({"workspaceId":workspace["id"],"status":"waiting_approval","after":workspace_first["nextAfter"],"targetBytes":target}),
    );
    assert_eq!(workspace_second, second);
    let invalid = peer.request(
        "task/listWorkspaceSummaries",
        json!({"workspaceId":workspace["id"],"targetBytes":1023}),
    );
    assert_eq!(invalid["error"]["code"], -32602);
    drop(peer);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn newest_workspace_summaries_resume_before_the_last_row_after_restart() {
    let root = std::env::temp_dir()
        .canonicalize()
        .unwrap()
        .join(format!("fliuno-recent-summary-{}", uuid::Uuid::new_v4()));
    let alternate = root.join("alternate");
    fs::create_dir_all(&alternate).unwrap();
    let db = root.join("kernel.db");
    let mut peer = Peer::start(&db);
    let workspace = peer.call("workspace/open", json!({"root":root}));
    let foreign = peer.call("workspace/open", json!({"root":alternate}));
    let create_session = |peer: &mut Peer, ws: &Value| {
        peer.call("session/create", json!({"workspaceId":ws["id"]}))["id"].clone()
    };
    let first = create_session(&mut peer, &workspace);
    let second = create_session(&mut peer, &workspace);
    let other = create_session(&mut peer, &foreign);
    let start = |peer: &mut Peer, session_id: &Value| {
        let task = peer.call(
            "task/start",
            json!({"sessionId":session_id,"input":"文档 ".repeat(80)}),
        );
        let id = task["id"].as_str().unwrap();
        peer.wait_for_status(id, "waiting_approval");
        task["id"].clone()
    };
    let oldest = start(&mut peer, &first);
    let excluded = start(&mut peer, &second);
    let _ = start(&mut peer, &other);
    let middle = start(&mut peer, &first);
    let newest = start(&mut peer, &second);
    peer.call("task/cancel", json!({"taskId":excluded}));
    let method = "task/listWorkspaceSummaries";
    let workspace_id = &workspace["id"];
    let before = peer.call(
        method,
        json!({"workspaceId":workspace_id,"status":"waiting_approval"}),
    );
    let ordered = before["items"].as_array().unwrap();
    assert_eq!(
        ordered.iter().map(|v| v["id"].clone()).collect::<Vec<_>>(),
        [oldest.clone(), middle.clone(), newest.clone()]
    );
    assert_eq!(
        peer.call(
            method,
            json!({"workspaceId":workspace_id,"status":"waiting_approval","order":"oldest_first"})
        ),
        before
    );
    let pair = vec![ordered[2].clone(), ordered[1].clone()];
    let bytes = serde_json::to_vec(&pair).unwrap().len();
    assert!(bytes >= 1024);
    let first_page = peer.call(method, json!({"workspaceId":workspace_id,"status":"waiting_approval","order":"newest_first","targetBytes":bytes}));
    assert_eq!(first_page["items"], json!(pair));
    assert_eq!(first_page["hasMore"], true);
    let newest_all = peer.call(
        method,
        json!({"workspaceId":workspace_id,"order":"newest_first"}),
    );
    assert_eq!(newest_all["items"][0]["id"], newest);
    assert_eq!(newest_all["items"][2]["id"], excluded);
    drop(peer);

    let mut peer = Peer::start(&db);
    let later = start(&mut peer, &second);
    let last_page = peer.call(method, json!({"workspaceId":workspace_id,"status":"waiting_approval","order":"newest_first","targetBytes":bytes,"after":first_page["nextAfter"]}));
    assert_eq!(last_page["items"], json!([ordered[0]]));
    assert_eq!(last_page["hasMore"], false);
    assert!(last_page["nextAfter"].as_u64().unwrap() < first_page["nextAfter"].as_u64().unwrap());
    let latest = peer.call(
        method,
        json!({"workspaceId":workspace_id,"order":"newest_first","limit":1}),
    );
    assert_eq!(latest["items"][0]["id"], later);
    assert_eq!(latest["hasMore"], true);
    drop(peer);
    fs::remove_dir_all(root).unwrap();
}
