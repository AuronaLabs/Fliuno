use super::*;
use fliuno_protocol::{MAX_TASK_SUMMARY_BATCH, TaskSummaryBatchParams, WorkSnapshotParams};

#[test]
fn task_summary_batch_reads_current_scoped_rows_in_input_order() {
    let root = std::env::temp_dir().join(format!(
        "fliuno-task-summary-batch-{}",
        uuid::Uuid::new_v4()
    ));
    fs::create_dir_all(root.join("other")).unwrap();
    let db = root.join("kernel.db");
    let mut peer = Peer::start(&db);
    assert_eq!(
        peer.call("task/getSummaries", json!({"taskIds":["missing"]})),
        json!({"items":[null],"throughSequence":0})
    );
    let local = peer.call("workspace/open", json!({"root":root}));
    let foreign = peer.call("workspace/open", json!({"root":root.join("other")}));
    let focus = peer.call("session/create", json!({"workspaceId":local["id"]}));
    let sibling = peer.call("session/create", json!({"workspaceId":local["id"]}));
    let other = peer.call("session/create", json!({"workspaceId":foreign["id"]}));
    let start = |peer: &mut Peer, session: &Value, input: &str| {
        let task = peer.call(
            "task/start",
            json!({"sessionId":session["id"],"input":input}),
        );
        let id = task["id"].as_str().unwrap().to_owned();
        peer.wait_for_status(&id, "waiting_approval");
        id
    };
    let selected = start(&mut peer, &focus, &"\u{6d4b}".repeat(250));
    let archived = start(&mut peer, &sibling, "make a second artifact");
    let excluded = start(&mut peer, &other, "make a foreign artifact");
    peer.call(
        "session/setArchived",
        json!({"sessionId":sibling["id"],"archived":true}),
    );
    drop(peer);

    let (server, mut client) = super::ag_ui_run::connect(&db);
    assert_eq!(client.info().capabilities["taskSummaryBatch"], true);
    assert_eq!(
        client.info().capabilities["maxTaskSummaryBatch"],
        MAX_TASK_SUMMARY_BATCH
    );
    let workspace_id = local["id"].as_str().unwrap().to_owned();
    let session_id = focus["id"].as_str().unwrap().to_owned();
    let mut view = client
        .subscribe_work(
            &WorkSnapshotParams {
                workspace_id: Some(workspace_id.clone()),
                session_id: None,
                approval_limit: 5,
                approval_target_bytes: None,
            },
            |_| {},
        )
        .unwrap();
    assert_eq!(view.snapshot.task_counts.waiting_approval, 2);
    let prior = view.events.cursor();
    let _: TaskSummary = client
        .call(
            "task/cancelSummary",
            &TaskIdParams {
                task_id: selected.clone(),
            },
            |note| {
                let _ = view.events.observe(note);
            },
        )
        .unwrap();
    let request = TaskSummaryBatchParams {
        task_ids: vec![
            archived.clone(),
            excluded.clone(),
            selected.clone(),
            "missing".into(),
            archived.clone(),
        ],
        workspace_id: Some(workspace_id),
        session_id: None,
    };
    let batch = client
        .get_task_summaries(&request, |note| {
            let _ = view.events.observe(note);
        })
        .unwrap();
    assert_eq!(batch.through_sequence, prior + 1);
    assert_eq!(
        batch.through_sequence,
        client.event_watermark(|_| {}).unwrap()
    );
    let archived_summary = batch.items[0].as_ref().unwrap();
    assert_eq!(archived_summary.id, archived);
    assert_eq!(archived_summary.status, TaskStatus::WaitingApproval);
    assert_eq!(archived_summary.pending_approval_count, 1);
    assert!(batch.items[1].is_none());
    let updated = batch.items[2].as_ref().unwrap();
    assert_eq!(updated.id, selected);
    assert_eq!(updated.status, TaskStatus::Cancelled);
    assert_eq!(updated.pending_approval_count, 0);
    assert_eq!(updated.input_preview.chars().count(), 160);
    assert!(batch.items[3].is_none());
    assert_eq!(
        serde_json::to_value(&batch.items[0]).unwrap(),
        serde_json::to_value(&batch.items[4]).unwrap()
    );
    let single: Value = client
        .call(
            "task/getSummary",
            &TaskIdParams {
                task_id: selected.clone(),
            },
            |_| {},
        )
        .unwrap();
    assert_eq!(serde_json::to_value(updated).unwrap(), single);

    let narrow = client
        .get_task_summaries(
            &TaskSummaryBatchParams {
                workspace_id: None,
                session_id: Some(session_id.clone()),
                ..request.clone()
            },
            |_| {},
        )
        .unwrap();
    assert!(
        narrow
            .items
            .iter()
            .enumerate()
            .all(|(i, row)| row.is_some() == (i == 2))
    );
    let all = client
        .get_task_summaries(
            &TaskSummaryBatchParams {
                workspace_id: None,
                ..request.clone()
            },
            |_| {},
        )
        .unwrap();
    assert_eq!(all.items[1].as_ref().unwrap().id, excluded);
    let limit = client
        .get_task_summaries(
            &TaskSummaryBatchParams {
                task_ids: vec![selected.clone(); MAX_TASK_SUMMARY_BATCH],
                ..request.clone()
            },
            |_| {},
        )
        .unwrap();
    assert!(
        limit
            .items
            .iter()
            .all(|row| row.as_ref().is_some_and(|row| row.id == selected))
    );

    for (params, expected) in [
        (json!({"taskIds":[]}), -32602),
        (
            json!({"taskIds":vec![selected.clone();MAX_TASK_SUMMARY_BATCH+1]}),
            -32602,
        ),
        (json!({"taskIds":[selected,"x".repeat(129)]}), -32602),
        (json!({"taskIds":["a b"]}), -32602),
        (
            json!({"taskIds":[selected],"sessionId":session_id,"workspaceId":request.workspace_id}),
            -32602,
        ),
        (json!({"taskIds":[selected],"sessionId":"missing"}), -32004),
        (
            json!({"taskIds":[selected],"workspaceId":"missing"}),
            -32004,
        ),
    ] {
        assert!(
            matches!(client.call::<_, Value>("task/getSummaries", &params, |_| {}), Err(Error::Rpc {code,..}) if code == expected),
            "unexpected result for {params}"
        );
    }
    drop(client);
    drop(server);
    fs::remove_dir_all(root).unwrap();
}
