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
                recent_task_limit: None,
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

#[test]
fn task_resume_status_batch_preserves_scope_and_read_only_recovery_policy() {
    use fliuno_protocol::{TaskResumeDisposition, TaskResumeStatus};
    use http_wire::{Fixture, Response};

    let root = std::env::temp_dir().join(format!("fliuno-resume-batch-{}", uuid::Uuid::new_v4()));
    fs::create_dir_all(root.join("outside")).unwrap();
    let db = root.join("kernel.db");
    let wire = Fixture::start(|request| {
        let body = if request.body["input"].as_array().unwrap().last().unwrap()["content"]
            == "complete"
        {
            json!({"status":"completed","output":[{"type":"message","content":[{"type":"output_text","text":"done"}]}]})
        } else {
            json!({"output":[]})
        };
        Response::json(200, body)
    });
    let mut peer = Peer::start_with_model_fixture(&db, Some(&wire.url("/v1/")));
    assert_eq!(
        peer.call("task/getResumeStatuses", json!({"taskIds":["missing"]})),
        json!({"items":[null],"throughSequence":0})
    );
    let ws = peer.call("workspace/open", json!({"root":root}));
    let outside = peer.call("workspace/open", json!({"root":root.join("outside")}));
    let focus = peer.call("session/create", json!({"workspaceId":ws["id"]}));
    let sibling = peer.call("session/create", json!({"workspaceId":ws["id"]}));
    let foreign = peer.call("session/create", json!({"workspaceId":outside["id"]}));
    let mut start = |session: &Value, input: &str, limit, status| {
        let task = peer.call(
            "task/startSummary",
            json!({
                "sessionId":session["id"],"input":input,"maxModelSteps":limit
            }),
        );
        let id = task["id"].as_str().unwrap();
        peer.wait_for_status(id, status);
        id.to_owned()
    };
    let uncertain = start(&focus, &"inspect\n".repeat(25_000), 2, "interrupted");
    let exhausted = start(&sibling, "exhaust budget", 1, "interrupted");
    let finished = start(&focus, "complete", 2, "completed");
    let excluded = start(&foreign, "complete", 2, "completed");
    drop(peer);

    let (server, mut client) = super::ag_ui_run::connect(&db);
    assert_eq!(client.info().capabilities["taskResumeStatusBatch"], true);
    assert_eq!(
        client.info().capabilities["maxTaskResumeStatusBatch"],
        MAX_TASK_SUMMARY_BATCH
    );
    let before = client.event_watermark(|_| {}).unwrap();
    let request = TaskSummaryBatchParams {
        workspace_id: Some(ws["id"].as_str().unwrap().to_owned()),
        session_id: None,
        task_ids: vec![
            exhausted.clone(),
            excluded,
            uncertain.clone(),
            "missing".into(),
            finished.clone(),
            uncertain.clone(),
        ],
    };
    let single = |client: &mut Client<_, _>, id| client.get_task_resume_status(id, |_| {}).unwrap();
    let exhausted_status = single(&mut client, &exhausted);
    let uncertain_status = single(&mut client, &uncertain);
    let finished_status = single(&mut client, &finished);
    assert_eq!(
        exhausted_status.disposition,
        TaskResumeDisposition::ModelStepBudgetExhausted
    );
    assert_eq!(
        uncertain_status.disposition,
        TaskResumeDisposition::RetryUncertainModelStep
    );
    assert_eq!(uncertain_status.remaining_model_steps, Some(1));
    assert_eq!(
        finished_status.disposition,
        TaskResumeDisposition::NotInterrupted
    );
    let batch = client.get_task_resume_statuses(&request, |_| {}).unwrap();
    assert_eq!(batch.through_sequence, before);
    assert_eq!(
        batch.items,
        vec![
            Some(exhausted_status),
            None,
            Some(uncertain_status.clone()),
            None,
            Some(finished_status.clone()),
            Some(uncertain_status.clone())
        ]
    );
    let session_id = focus["id"].as_str().unwrap().to_owned();
    let narrow = client
        .get_task_resume_statuses(
            &TaskSummaryBatchParams {
                workspace_id: None,
                session_id: Some(session_id.clone()),
                ..request.clone()
            },
            |_| {},
        )
        .unwrap();
    assert_eq!(
        narrow.items,
        vec![
            None,
            None,
            Some(uncertain_status.clone()),
            None,
            Some(finished_status),
            Some(uncertain_status)
        ]
    );
    for invalid in [
        json!({"taskIds":[]}),
        json!({"taskIds":vec![&uncertain; MAX_TASK_SUMMARY_BATCH + 1]}),
        json!({"taskIds":["bad id"]}),
        json!({"taskIds":[uncertain],"workspaceId":ws["id"],"sessionId":session_id}),
    ] {
        assert!(matches!(
            client.call::<_, Value>("task/getResumeStatuses", &invalid, |_| {}),
            Err(Error::Rpc { code: -32602, .. })
        ));
    }
    assert!(matches!(
        client.call::<_, Value>(
            "task/getResumeStatuses",
            &json!({"taskIds":[uncertain],"sessionId":"missing"}),
            |_| {}
        ),
        Err(Error::Rpc { code: -32004, .. })
    ));
    assert!(matches!(
        client.call::<_, TaskResumeStatus>("task/resume", &json!({"taskId":uncertain}), |_| {}),
        Err(Error::Rpc { code: -32009, .. })
    ));
    assert_eq!(client.event_watermark(|_| {}).unwrap(), before);
    assert_eq!(wire.requests().len(), 4);
    drop(client);
    drop(server);
    fs::remove_dir_all(root).unwrap();
}
