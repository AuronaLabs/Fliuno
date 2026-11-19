use super::*;
use fliuno_protocol::{MAX_TASK_CANCEL_BATCH, TaskCancelBatchParams, TaskCancelScope};
use http_wire::{Fixture, Response};

#[test]
fn cancel_batch_rolls_back_on_scope_or_storage_failure_and_retries_after_restart() {
    let root = std::env::temp_dir().join(format!("fliuno-cancel-batch-{}", uuid::Uuid::new_v4()));
    fs::create_dir_all(root.join("other")).unwrap();
    let models = Fixture::start(|req| {
        let input = req.body["input"].as_array().unwrap().last().unwrap();
        let output = match input["content"].as_str().unwrap() {
            "block" => return Response::hold(),
            "complete" => {
                json!([{"type":"message","content":[{"type":"output_text","text":"done"}]}])
            }
            _ => json!([{"type":"function_call","call_id":"create","name":"artifact_create",
                "arguments":"{\"title\":\"test\",\"content\":\"test\"}"}]),
        };
        Response::json(200, json!({"status":"completed","output":output}))
    });
    let db = root.join("kernel.db");
    let mut peer = Peer::start_with_model_fixture(&db, Some(&models.url("/v1/")));
    let workspace = peer.call("workspace/open", json!({"root":root}));
    let other = peer.call("workspace/open", json!({"root":root.join("other")}));
    let focus = peer.call("session/create", json!({"workspaceId":workspace["id"]}));
    let sibling = peer.call("session/create", json!({"workspaceId":workspace["id"]}));
    let foreign = peer.call("session/create", json!({"workspaceId":other["id"]}));
    let start = |peer: &mut Peer, session: &Value, input| {
        peer.call(
            "task/startSummary",
            json!({"sessionId":session["id"],"input":input,"context":{"mode":"isolated"}}),
        )["id"]
            .as_str()
            .unwrap()
            .to_owned()
    };
    let pending = start(&mut peer, &focus, "pending");
    let approval =
        peer.wait_for_status(&pending, "waiting_approval")["pendingApproval"]["id"].clone();
    let done = start(&mut peer, &focus, "complete");
    peer.wait_for_status(&done, "completed");
    let excluded = start(&mut peer, &foreign, "other workspace");
    peer.wait_for_status(&excluded, "waiting_approval");
    let running = start(&mut peer, &sibling, "block");
    let deadline = Instant::now() + Duration::from_secs(5);
    while !models
        .requests()
        .iter()
        .any(|req| req.body["input"].as_array().unwrap().last().unwrap()["content"] == "block")
    {
        assert!(Instant::now() < deadline, "model request was not sent");
        thread::sleep(Duration::from_millis(5));
    }
    let workspace_params = json!({"workspaceId":workspace["id"]});
    let before = peer.call("work/getSnapshot", workspace_params.clone());
    let cursor = before["approvals"]["snapshot"]["throughSequence"]
        .as_u64()
        .unwrap();
    let scope = json!({"kind":"workspace","workspaceId":workspace["id"]});
    for params in [
        json!({"scope":scope,"taskIds":[pending,excluded]}),
        json!({"scope":scope,"taskIds":[pending,"missing-task"]}),
        json!({"scope":{"kind":"session","sessionId":focus["id"]},"taskIds":[pending,running]}),
    ] {
        assert_eq!(
            peer.request("task/cancelBatch", params)["error"]["code"],
            -32004
        );
    }
    for params in [
        json!({"scope":scope,"taskIds":[]}),
        json!({"scope":scope,"taskIds":[pending,pending]}),
        json!({"scope":scope,"taskIds":[pending,"non printable"]}),
        json!({"scope":scope,"taskIds":vec![&pending;MAX_TASK_CANCEL_BATCH+1]}),
    ] {
        assert_eq!(
            peer.request("task/cancelBatch", params)["error"]["code"],
            -32602
        );
    }
    let batch = json!({"scope":scope,"taskIds":[pending,done,running]});
    let db_conn = rusqlite::Connection::open(&db).unwrap();
    db_conn.execute_batch("CREATE TRIGGER fail_batch_update BEFORE UPDATE ON tasks
        WHEN json_extract(OLD.data,'$.status')='running' AND json_extract(NEW.data,'$.status')='cancelled'
        BEGIN SELECT RAISE(ABORT,'injected batch test failure'); END;").unwrap();
    assert_eq!(
        peer.request("task/cancelBatch", batch.clone())["error"]["code"],
        -32603
    );
    let after_failure = peer.call("work/getSnapshot", workspace_params.clone());
    assert_eq!(after_failure["taskCounts"], before["taskCounts"]);
    assert_eq!(
        after_failure["approvals"]["page"]["items"],
        before["approvals"]["page"]["items"]
    );
    assert_eq!(
        after_failure["approvals"]["snapshot"]["throughSequence"],
        cursor
    );
    assert!(
        peer.call(
            "events/listWorkspace",
            json!({"workspaceId":workspace["id"],"after":cursor})
        )["events"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        peer.call("task/getSummary", json!({"taskId":running}))["status"],
        "running"
    );
    db_conn
        .execute_batch("DROP TRIGGER fail_batch_update")
        .unwrap();
    drop(db_conn);

    let receipt = peer.call("task/cancelBatch", batch.clone());
    assert_eq!(receipt["throughSequence"], cursor + 2);
    let receipts = receipt["items"].as_array().unwrap();
    for (row, (id, state, changed)) in receipts.iter().zip([
        (&pending, "cancelled", true),
        (&done, "completed", false),
        (&running, "cancelled", true),
    ]) {
        assert_eq!(row["task"]["id"].as_str(), Some(id.as_str()));
        assert_eq!(row["task"]["status"], state);
        assert_eq!(row["task"]["pendingApprovalCount"], 0);
        assert_eq!(row["changed"], changed);
    }
    let subsequent = peer.call("work/getSnapshot", workspace_params);
    assert_eq!(subsequent["taskCounts"]["cancelled"], 2);
    assert_eq!(subsequent["taskCounts"]["running"], 0);
    assert_eq!(subsequent["taskCounts"]["waitingApproval"], 0);
    assert!(
        subsequent["approvals"]["page"]["items"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        peer.call("task/getSummary", json!({"taskId":excluded}))["status"],
        "waiting_approval"
    );
    let replay = peer.call(
        "events/listWorkspace",
        json!({"workspaceId":workspace["id"],"after":cursor,"through":receipt["throughSequence"]}),
    );
    let events = replay["events"].as_array().unwrap();
    assert_eq!(events.len(), 2);
    for (evt, id) in events.iter().zip([&pending, &running]) {
        assert_eq!(evt["taskId"].as_str(), Some(id.as_str()));
        assert_eq!(evt["data"]["type"], "task_cancelled");
    }
    assert!(
        peer.request(
            "approval/respond",
            json!({"taskId":pending,"approvalId":approval,"decision":"allow_once"})
        )["error"]
            .is_object()
    );
    drop(peer);

    let (server, mut client) = super::ag_ui_run::connect(&db);
    assert_eq!(client.info().capabilities["taskCancelBatch"], true);
    assert_eq!(
        client.info().capabilities["maxTaskCancelBatch"],
        MAX_TASK_CANCEL_BATCH
    );
    let request = TaskCancelBatchParams {
        scope: TaskCancelScope::Workspace {
            workspace_id: workspace["id"].as_str().unwrap().into(),
        },
        task_ids: vec![pending, done, running],
    };
    let before_retry = client.event_watermark(|_| {}).unwrap();
    let typed = client.cancel_task_batch(&request, |_| {}).unwrap();
    assert!(typed.items.iter().all(|item| !item.changed));
    assert_eq!(typed.through_sequence, before_retry);
    assert_eq!(client.event_watermark(|_| {}).unwrap(), before_retry);
    drop(client);
    drop(server);
    fs::remove_dir_all(root).unwrap();
}
