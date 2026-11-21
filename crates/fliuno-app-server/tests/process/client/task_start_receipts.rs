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
        67
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
    let batch = peer.call(
        "task/getSummariesByRequest",
        json!({"sessionId":session_id,"clientRequestIds":["compact","missing","legacy"]}),
    );
    assert_eq!(batch["items"][0], receipt);
    assert!(batch["items"][1].is_null());
    assert_eq!(batch["items"][2], legacy_retry);
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

#[test]
fn image_receipt_reuses_valid_input_but_rejects_malformed_and_changed_parts() {
    const PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+yhE0AAAAASUVORK5CYII=";
    let root = std::env::temp_dir().join(format!("fliuno-image-receipt-{}", uuid::Uuid::new_v4()));
    fs::create_dir(&root).unwrap();
    let path = root.join("kernel.db");
    let mut peer = Peer::start(&path);
    let workspace = peer.call("workspace/open", json!({"root":root}));
    let session = peer.call("session/create", json!({"workspaceId":workspace["id"]}));
    let mut request = json!({
        "sessionId":session["id"], "clientRequestId":"image",
        "input":[{"type":"image", "mimeType":"image/png", "data":"invalid"}]
    });
    assert_eq!(
        peer.request("task/startSummary", request.clone())["error"]["code"],
        -32602
    );
    let mut oversized = request.clone();
    oversized["input"] = json!(vec![json!({"type":"text", "text":""}); 33]);
    oversized["input"][32] = json!({"type":"image", "mimeType":"image/png", "data":PNG});
    assert_eq!(
        peer.request("task/startSummary", oversized)["error"]["code"],
        -32602
    );
    let mut limit = request.clone();
    limit["clientRequestId"] = json!("exact-part-limit");
    limit["input"] = json!(vec![json!({"type":"text", "text":""}); 32]);
    limit["input"][31] = json!({"type":"image", "mimeType":"image/png", "data":PNG});
    assert_eq!(
        peer.call("task/startSummary", limit)["inputPreview"],
        "[image]"
    );
    request["input"][0]["data"] = json!(PNG);
    let started = peer.call("task/startSummary", request.clone());
    let task_id = started["id"].as_str().unwrap();
    peer.wait_for_status(task_id, "waiting_approval");
    drop(peer);
    let mut peer = Peer::start(&path);
    let summary = peer.call("task/startSummary", request.clone());
    assert_eq!(summary["id"], task_id);
    assert_eq!(summary["inputPreview"], "[image]");
    assert_eq!(summary["status"], "waiting_approval");
    let mut changed = request.clone();
    changed["input"][0]["data"] = json!("iVBORw0KGgo=");
    assert_eq!(
        peer.request("task/startSummary", changed)["error"]["code"],
        -32009
    );
    request["input"][0]["mimeType"] = json!("image/jpeg");
    assert_eq!(
        peer.request("task/startSummary", request)["error"]["code"],
        -32602
    );
    drop(peer);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn request_summary_batch_reconciles_keys_in_one_session_after_restart() {
    use fliuno_protocol::{MAX_TASK_SUMMARY_BATCH, TaskRequestSummaryBatchParams};

    let temp_dir = std::env::temp_dir().canonicalize().unwrap();
    let root = temp_dir.join(format!(
        "fliuno-request-summary-batch-{}",
        uuid::Uuid::new_v4()
    ));
    fs::create_dir(&root).unwrap();
    let db = root.join("kernel.db");
    let mut peer = Peer::start(&db);
    let ws = peer.call("workspace/open", json!({"root":root}));
    let session = peer.call("session/create", json!({"workspaceId":ws["id"]}));
    let other = peer.call("session/create", json!({"workspaceId":ws["id"]}));
    let start = |peer: &mut Peer, session: &Value, key: &str| {
        let task = peer.call(
            "task/startSummary",
            json!({"sessionId":session["id"],"input":"draft report","clientRequestId":key}),
        );
        peer.wait_for_status(task["id"].as_str().unwrap(), "waiting_approval");
        task["id"].as_str().unwrap().to_owned()
    };
    let waiting = start(&mut peer, &session, "shared");
    let cancelled = start(&mut peer, &session, "cancelled");
    let foreign_shared = start(&mut peer, &other, "shared");
    start(&mut peer, &other, "other-only");
    peer.call("task/cancelSummary", json!({"taskId":cancelled}));
    drop(peer);

    let (server, mut client) = super::ag_ui_run::connect(&db);
    assert_eq!(
        client.info().capabilities["taskSummaryByRequestBatch"],
        true
    );
    assert_eq!(
        client.info().capabilities["maxTaskSummaryByRequestBatch"],
        MAX_TASK_SUMMARY_BATCH
    );
    let request = TaskRequestSummaryBatchParams {
        session_id: session["id"].as_str().unwrap().to_owned(),
        client_request_ids: ["cancelled", "other-only", "shared", "missing", "cancelled"]
            .map(str::to_owned)
            .to_vec(),
    };
    let before = client.event_watermark(|_| {}).unwrap();
    let batch = client
        .get_task_summaries_by_request(&request, |_| {})
        .unwrap();
    assert_eq!(batch.through_sequence, before);
    for i in [0, 4] {
        let summary = batch.items[i].as_ref().unwrap();
        assert_eq!(summary.id, cancelled);
        assert_eq!(summary.status, TaskStatus::Cancelled);
    }
    let summary = batch.items[2].as_ref().unwrap();
    assert_eq!(summary.id, waiting);
    assert_eq!(summary.status, TaskStatus::WaitingApproval);
    assert!(batch.items[1].is_none() && batch.items[3].is_none());
    let other_request = TaskRequestSummaryBatchParams {
        session_id: other["id"].as_str().unwrap().to_owned(),
        client_request_ids: vec!["shared".into()],
    };
    let other_batch = client
        .get_task_summaries_by_request(&other_request, |_| {})
        .unwrap();
    assert_eq!(other_batch.items[0].as_ref().unwrap().id, foreign_shared);

    for keys in [
        vec![],
        vec!["contains space".into()],
        vec!["missing".into(); MAX_TASK_SUMMARY_BATCH + 1],
    ] {
        let bad = TaskRequestSummaryBatchParams {
            client_request_ids: keys,
            ..request.clone()
        };
        assert!(matches!(
            client.get_task_summaries_by_request(&bad, |_| {}),
            Err(fliuno_client::Error::Rpc { code: -32602, .. })
        ));
    }
    let missing_session = TaskRequestSummaryBatchParams {
        session_id: "missing-session".into(),
        ..request
    };
    assert!(matches!(
        client.get_task_summaries_by_request(&missing_session, |_| {}),
        Err(fliuno_client::Error::Rpc { code: -32004, .. })
    ));
    assert_eq!(client.event_watermark(|_| {}).unwrap(), before);
    drop(client);
    drop(server);
    let root = root.canonicalize().unwrap();
    assert_eq!(root.parent(), Some(temp_dir.as_path()));
    fs::remove_dir_all(root).unwrap();
}
