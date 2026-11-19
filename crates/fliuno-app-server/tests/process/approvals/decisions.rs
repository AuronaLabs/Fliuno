use super::*;

#[test]
fn batch_receipts_preserve_order_and_pair_identity_after_restart() {
    let root = std::env::temp_dir()
        .canonicalize()
        .unwrap()
        .join(format!("fliuno-decision-batch-{}", uuid::Uuid::new_v4()));
    fs::create_dir(&root).unwrap();
    let db = root.join("kernel.db");
    let mut peer = Peer::start(&db);
    let workspace = peer.call("workspace/open", json!({"root":root}));
    let session = peer.call("session/create", json!({"workspaceId":workspace["id"]}));
    let mut approvals = Vec::new();
    let mut tasks = Vec::new();
    for input in [
        "record a denial",
        "record a grant",
        "leave pending",
        "cancel before decision",
    ] {
        let task = peer.call(
            "task/start",
            json!({"sessionId":session["id"],"input":input}),
        );
        let id = task["id"].as_str().unwrap();
        let task = peer.wait_for_status(id, "waiting_approval");
        approvals.push(json!({"taskId":id,"approvalId":task["pendingApproval"]["id"]}));
        tasks.push(id.to_owned());
    }
    let before = peer.call("events/getWatermark", json!({}))["throughSequence"].clone();
    let request = |keys: &[Value]| json!({"approvals":keys});
    assert_eq!(
        peer.call("approval/getDecisions", request(&approvals)),
        json!({
            "items":[null,null,null,null], "throughSequence":before
        })
    );
    let mut receipts = Vec::new();
    for ((key, id), (decision, status)) in approvals
        .iter()
        .zip(&tasks)
        .zip([("deny", "failed"), ("allow_once", "completed")])
    {
        let mut resolve = key.clone();
        resolve["decision"] = json!(decision);
        let event = peer.call("approval/respondDecision", resolve);
        peer.wait_for_status(id, status);
        let mut receipt = key.clone();
        receipt["sequence"] = event["sequence"].clone();
        receipt["decision"] = json!(decision);
        receipts.push(receipt);
    }
    peer.call("task/cancel", json!({"taskId":tasks[3]}));
    let crossed = json!({"taskId":tasks[0],"approvalId":approvals[1]["approvalId"]});
    let query = json!({"approvals":[approvals[1],crossed,approvals[0],approvals[2],approvals[1],approvals[3],crossed]});
    let watermark = peer.call("events/getWatermark", json!({}))["throughSequence"].clone();
    let expected = json!({
        "items":[receipts[1],null,receipts[0],null,receipts[1],null,null],
        "throughSequence":watermark
    });
    assert_eq!(peer.call("approval/getDecisions", query.clone()), expected);
    for invalid in [
        json!({"approvals":[]}),
        json!({"approvals":vec![approvals[0].clone();65]}),
        json!({"approvals":[approvals[0],{"taskId":"","approvalId":"valid"}]}),
        json!({"approvals":[{"taskId":"valid","approvalId":"bad key"}]}),
        json!({"approvals":[{"taskId":"valid","approvalId":"x".repeat(129)}]}),
    ] {
        assert_eq!(
            peer.request("approval/getDecisions", invalid)["error"]["code"],
            -32602
        );
    }
    let max_page = peer.call(
        "approval/getDecisions",
        json!({"approvals":vec![approvals[0].clone();64]}),
    );
    assert_eq!(max_page["items"], json!(vec![receipts[0].clone(); 64]));
    assert_eq!(
        peer.call("events/getWatermark", json!({}))["throughSequence"],
        watermark
    );
    drop(peer);

    let mut reopened = Peer::start(&db);
    assert_eq!(reopened.call("approval/getDecisions", query), expected);
    assert_eq!(
        reopened
            .call("artifacts/list", json!({"taskId":tasks[1]}))
            .as_array()
            .unwrap()
            .len(),
        1
    );
    drop(reopened);
    fs::remove_dir_all(root).unwrap();
}
