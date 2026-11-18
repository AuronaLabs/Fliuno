//! A real isolated ACP peer receives the same history as the model backend.
use super::*;

#[test]
fn model_and_acp_tasks_share_completed_history_without_reusing_permissions() {
    let directory = Directory::new();
    let mut peer = Peer::start(&directory);
    let session = peer.session(&directory);
    let model = peer.call(
        "task/start",
        json!({"sessionId":session,"input":"remember this report"}),
    );
    let waiting = peer.wait(&model["id"], |t| t["status"] == "waiting_approval");
    let old_approval = waiting["pendingApproval"]["id"].clone();
    peer.call(
        "approval/respond",
        json!({"taskId":model["id"],"approvalId":old_approval,"decision":"allow_once"}),
    );
    peer.wait(&model["id"], |t| t["status"] == "completed");
    let probe = peer.start_task(&session, "context_probe", "after-model");
    let completed = peer.wait(&probe["id"], |t| t["status"] == "completed");
    let report: Value = serde_json::from_str(
        completed["output"]
            .as_str()
            .unwrap()
            .strip_prefix("ready")
            .unwrap(),
    )
    .unwrap();
    assert_eq!(report["current"], "context_probe");
    assert_eq!(report["context"], probe["context"]);
    assert_eq!(report["context"]["turns"][0]["taskId"], model["id"]);
    assert_eq!(report["context"]["turns"][0]["backend"]["type"], "model");
    assert!(
        report["context"]
            .to_string()
            .contains("remember this report")
    );
    assert!(!report.to_string().contains(old_approval.as_str().unwrap()));
    let next = peer.call(
        "task/start",
        json!({"sessionId":session,"input":"follow up on both tasks"}),
    );
    let turns = next["context"]["turns"].as_array().unwrap();
    assert_eq!(turns.len(), 2);
    assert_eq!(turns[1]["taskId"], probe["id"]);
    assert_eq!(turns[1]["backend"]["type"], "external_agent");
    let waiting = peer.wait(&next["id"], |t| t["status"] == "waiting_approval");
    assert_ne!(waiting["pendingApproval"]["id"], old_approval);
    assert!(
        peer.request(
            "approval/respond",
            json!({"taskId":next["id"],"approvalId":old_approval,"decision":"allow_once"})
        )
        .get("error")
        .is_some()
    );
    assert!(
        peer.call("artifacts/list", json!({"taskId":next["id"]}))
            .as_array()
            .unwrap()
            .is_empty()
    );
    peer.call("task/cancel", json!({"taskId":next["id"]}));
    peer.stop();
}
