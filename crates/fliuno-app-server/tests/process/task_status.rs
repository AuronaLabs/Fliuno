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
