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
    let foreign = peer.call("session/create", json!({"workspaceId":workspace["id"]}));
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
    let _foreign = start(&foreign, "other session");
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
    drop(peer);
    fs::remove_dir_all(root).unwrap();
}
