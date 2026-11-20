use super::*;

#[test]
fn pending_approval_details_batch_preserves_order_and_rechecks_live_state() {
    let root =
        std::env::temp_dir().join(format!("fliuno-approval-details-{}", uuid::Uuid::new_v4()));
    fs::create_dir(&root).unwrap();
    let db = root.join("kernel.db");
    let mut peer = Peer::start(&db);
    let workspace = peer.call("workspace/open", json!({"root":root}));
    let session = peer.call("session/create", json!({"workspaceId":workspace["id"]}));
    let mut keys = Vec::new();
    for input in ["batch one", "batch two", "batch three"] {
        let task = peer.call(
            "task/startSummary",
            json!({"sessionId":session["id"],"input":input}),
        );
        let waiting = peer.wait_for_status(task["id"].as_str().unwrap(), "waiting_approval");
        keys.push(json!({
            "taskId":task["id"],
            "approvalId":waiting["pendingApproval"]["id"]
        }));
    }
    let single = peer.call("approval/get", keys[0].clone());
    let batch = peer.call(
        "approval/getBatch",
        json!({"approvals":[keys[2].clone(),keys[0].clone(),keys[2].clone(),
            {"taskId":keys[0]["taskId"],"approvalId":keys[1]["approvalId"]}]}),
    );
    assert_eq!(batch["items"].as_array().unwrap().len(), 4);
    assert_eq!(
        batch["items"][0],
        peer.call("approval/get", keys[2].clone())
    );
    assert_eq!(batch["items"][1], single);
    assert_eq!(batch["items"][2], batch["items"][0]);
    assert_eq!(batch["items"][3], Value::Null);

    peer.call(
        "approval/respond",
        json!({"taskId":keys[0]["taskId"],"approvalId":keys[0]["approvalId"],"decision":"allow_once"}),
    );
    peer.wait_for_status(keys[0]["taskId"].as_str().unwrap(), "completed");
    let after = peer.call(
        "approval/getBatch",
        json!({"approvals":[keys[0].clone(),keys[1].clone()]}),
    );
    assert_eq!(after["items"][0], Value::Null);
    assert_eq!(
        after["items"][1],
        peer.call("approval/get", keys[1].clone())
    );
    for invalid in [
        json!({"approvals":[]} ),
        json!({"approvals":vec![keys[0].clone();33]}),
        json!({"approvals":[{"taskId":"中","approvalId":"ok"}]}),
    ] {
        assert_eq!(
            peer.request("approval/getBatch", invalid)["error"]["code"],
            -32602
        );
    }
    drop(peer);
    let mut reopened = Peer::start(&db);
    let resumed = reopened.call(
        "approval/getBatch",
        json!({"approvals":[keys[1].clone(),keys[2].clone()]}),
    );
    assert!(resumed["items"][0].is_object());
    assert!(resumed["items"][1].is_object());
    drop(reopened);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn detail_batch_charges_repeat_positions_and_rejects_one_oversized_item() {
    let root =
        std::env::temp_dir().join(format!("fliuno-approval-budget-{}", uuid::Uuid::new_v4()));
    fs::create_dir(&root).unwrap();
    let db = root.join("kernel.db");
    let mut peer = Peer::start(&db);
    let workspace = peer.call("workspace/open", json!({"root":root}));
    let session = peer.call("session/create", json!({"workspaceId":workspace["id"]}));
    let mut make_key = |input: String| {
        let task = peer.call(
            "task/startSummary",
            json!({"sessionId":session["id"], "input":input,"context":{"mode":"isolated"}}),
        );
        let task_id = task["id"].as_str().unwrap();
        let waiting = peer.wait_for_status(task_id, "waiting_approval");
        json!({"taskId":task_id,"approvalId":waiting["pendingApproval"]["id"]})
    };
    let medium = make_key("界\n\\\"".repeat(12_000));
    let huge = make_key("x".repeat(262_144));
    let one = peer.call("approval/get", medium.clone());
    let two = peer.call("approval/getBatch", json!({"approvals":[medium,medium]}));
    assert_eq!(two["items"], json!([one, one]));
    for keys in [json!([medium, medium, medium]), json!([huge])] {
        assert_eq!(
            peer.request("approval/getBatch", json!({"approvals":keys}))["error"]["code"],
            -32602
        );
    }
    assert_eq!(
        peer.call("approval/get", huge)["approval"]["call"]["arguments"]["content"]
            .as_str()
            .unwrap()
            .len(),
        262_144
    );
    drop(peer);
    fs::remove_dir_all(root).unwrap();
}
