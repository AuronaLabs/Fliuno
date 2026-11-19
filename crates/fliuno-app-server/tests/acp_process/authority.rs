//! A queued registration mismatch must be visible through the real scheduler,
//! rather than only refusing IO and leaving the task permanently running.
use super::*;

#[test]
fn changed_queued_registration_refuses_resume_and_a_fresh_task_can_run() {
    let directory = Directory::new();
    let config = fs::read(directory.0.join("agents.json")).unwrap();
    let mut kernel = fliuno_core::Kernel::open(
        directory.0.join("kernel.db"),
        Box::new(fliuno_core::DemoProvider),
    )
    .unwrap();
    fliuno_agents::register_config(&mut kernel, serde_json::from_slice(&config).unwrap()).unwrap();
    let workspace = kernel
        .open_workspace(directory.0.to_str().unwrap())
        .unwrap();
    let session = kernel.create_session(&workspace.id).unwrap();
    let original = kernel
        .create_task_with_backend(
            &session.id,
            "context_probe",
            Some("original-submission"),
            fliuno_protocol::TaskBackend::ExternalAgent {
                agent_id: "fixture".into(),
            },
        )
        .unwrap()
        .0;
    // No worker has ever received this task. Restart with a new filesystem
    // policy while retaining the same Agent ID and submission identity.
    drop(kernel);
    directory.workspace_access();
    let mut peer = Peer::start(&directory);
    let duplicate = peer.start_task(&session.id, "context_probe", "original-submission");
    assert_eq!(duplicate["id"], original.id);
    assert_eq!(
        duplicate["backendAuthority"],
        json!(original.backend_authority)
    );
    let reference = json!({"taskId":original.id});
    let before = peer.call("events/list", reference.clone());
    assert_eq!(
        peer.call("task/getResumeStatus", reference.clone()),
        json!({
            "taskId":original.id,"status":"interrupted","disposition":"agent_registration_changed","remainingModelSteps":null
        })
    );
    let refused = peer.request("task/resume", reference.clone());
    assert_eq!(refused["error"]["code"], -32009);
    assert!(
        refused["error"]["message"]
            .as_str()
            .unwrap()
            .contains("registration changed")
    );
    assert_eq!(peer.call("task/get", reference.clone()), duplicate);
    assert_eq!(peer.call("events/list", reference.clone()), before);
    let fresh = peer.start_task(&session.id, "context_probe", "fresh-submission");
    assert_ne!(fresh["backendAuthority"], duplicate["backendAuthority"]);
    peer.wait(&fresh["id"], |task| task["status"] == "completed");
    peer.stop();
    let mut peer = Peer::start(&directory);
    assert_eq!(peer.call("task/get", reference), duplicate);
    peer.stop();
}
