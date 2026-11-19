use super::*;

#[test]
fn summary_retry_receipts_survive_restart_and_commit_with_the_task_and_first_event() {
    let root = std::env::temp_dir().join(format!("fliuno-start-receipt-{}", uuid::Uuid::new_v4()));
    fs::create_dir(&root).unwrap();
    let db_path = root.join("kernel.db");
    let mut peer = Peer::start(&db_path);
    let ws = peer.call("workspace/open", json!({"root":root}));
    let session = peer.call("session/create", json!({"workspaceId":ws["id"]}));
    let session_id = session["id"].as_str().unwrap();
    let legacy_request =
        json!({"sessionId":session_id,"input":"legacy approval","clientRequestId":"legacy"});
    let legacy = peer.call("task/startSummary", legacy_request.clone());
    peer.wait_for_status(legacy["id"].as_str().unwrap(), "waiting_approval");
    drop(peer);
    let db = rusqlite::Connection::open(&db_path).unwrap();
    db.execute_batch("DROP TABLE task_start_receipts; PRAGMA user_version=43;")
        .unwrap();

    let mut peer = Peer::start(&db_path);
    assert_eq!(
        db.pragma_query_value::<u32, _>(None, "user_version", |row| row.get(0))
            .unwrap(),
        45
    );
    assert_eq!(
        db.query_row::<i64, _, _>("SELECT COUNT(*) FROM task_start_receipts", [], |row| row
            .get(0))
            .unwrap(),
        0
    );
    let legacy_retry = peer.call("task/startSummary", legacy_request.clone());
    assert_eq!(legacy_retry["id"], legacy["id"]);
    assert_eq!(legacy_retry["status"], "waiting_approval");
    let mut legacy_conflict = legacy_request;
    legacy_conflict["input"] = json!("different legacy input");
    assert_eq!(
        peer.request("task/startSummary", legacy_conflict)["error"]["code"],
        -32009
    );

    let rows = || {
        db.query_row::<(i64,i64,i64,i64), _, _>(
        "SELECT (SELECT COUNT(*) FROM tasks),(SELECT COUNT(*) FROM task_requests),(SELECT COUNT(*) FROM task_start_receipts),(SELECT COUNT(*) FROM events)",
        [], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
    ).unwrap()
    };
    let before = rows();
    db.execute_batch(
        "CREATE TRIGGER fail_start_receipt BEFORE INSERT ON task_start_receipts
        BEGIN SELECT RAISE(ABORT,'receipt test fault'); END;",
    )
    .unwrap();
    let new_request =
        json!({"sessionId":session_id,"input":"待审阅".repeat(4000),"clientRequestId":"compact"});
    assert_eq!(
        peer.request("task/startSummary", new_request.clone())["error"]["code"],
        -32603
    );
    assert_eq!(rows(), before);
    assert_eq!(
        peer.request(
            "task/getSummaryByRequest",
            json!({"sessionId":session_id,"clientRequestId":"compact"})
        )["error"]["code"],
        -32004
    );
    db.execute_batch("DROP TRIGGER fail_start_receipt;")
        .unwrap();
    let created = peer.call("task/startSummary", new_request.clone());
    let id = created["id"].as_str().unwrap();
    let pending = peer.wait_for_status(id, "waiting_approval");
    peer.call(
        "approval/respond",
        json!({"taskId":id,"approvalId":pending["pendingApproval"]["id"],"decision":"allow_once"}),
    );
    peer.wait_for_status(id, "completed");
    drop(peer);

    let mut peer = Peer::start(&db_path);
    let stored: String = db
        .query_row("SELECT data FROM tasks WHERE id=?1", [id], |row| row.get(0))
        .unwrap();
    assert!(stored.len() > 32_000);
    // Damage only the isolated, already completed test fixture to prove an
    // indexed retry never attempts to deserialize the full snapshot.
    db.execute(
        "UPDATE tasks SET data='unreadable task body' WHERE id=?1",
        [id],
    )
    .unwrap();
    assert_eq!(
        peer.request("task/get", json!({"taskId":id}))["error"]["code"],
        -32603
    );
    let mut retry = new_request.clone();
    retry["context"] = json!({});
    let receipt = peer.call("task/startSummary", retry);
    assert_eq!(receipt["id"], id);
    assert_eq!(receipt["status"], "completed");
    assert_eq!(
        receipt,
        peer.call(
            "task/getSummaryByRequest",
            json!({"sessionId":session_id,"clientRequestId":"compact"})
        )
    );
    for (field, value) in [
        ("input", json!("待审阅".repeat(4000) + "!")),
        ("context", json!({"mode":"isolated"})),
        (
            "backend",
            json!({"type":"external_agent","agentId":"missing"}),
        ),
    ] {
        let mut changed = new_request.clone();
        changed[field] = value;
        assert_eq!(
            peer.request("task/startSummary", changed)["error"]["code"],
            -32009
        );
    }
    db.execute(
        "UPDATE tasks SET data=?1 WHERE id=?2",
        rusqlite::params![stored, id],
    )
    .unwrap();
    let full = peer.call("task/start", new_request);
    assert_eq!(full["id"], id);
    assert_eq!(full["status"], "completed");
    let page = peer.call("events/list", json!({"taskId":id}));
    assert_eq!(
        page["events"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|ev| ev["data"]["type"] == "task_started")
            .count(),
        1
    );
    drop(peer);
    drop(db);
    fs::remove_dir_all(root).unwrap();
}
