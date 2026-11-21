//! Exercise actual adapter registrations without executing a remote task.
use fliuno_core::{CancellationToken, DemoProvider, Kernel};
use fliuno_protocol::{TaskBackend, TaskResumeDisposition, TaskStatus};
use serde_json::{Value, json};
use std::sync::atomic::{AtomicUsize, Ordering};

static NEXT_FIXTURE: AtomicUsize = AtomicUsize::new(0);

fn cannot_rebind(before: Value, after: Value, acp: bool) {
    let directory = std::env::temp_dir().join(format!(
        "fliuno-agent-identity-{}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
        NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir(&directory).unwrap();
    let database = directory.join("kernel.db");
    let register = |kernel: &mut Kernel, config: Value| {
        if acp {
            fliuno_agents::register_config(kernel, serde_json::from_value(config).unwrap())
                .unwrap();
        } else {
            fliuno_agents::register_a2a_config(kernel, serde_json::from_value(config).unwrap())
                .unwrap();
        }
    };
    let mut kernel = Kernel::open(&database, Box::new(DemoProvider)).unwrap();
    register(&mut kernel, before);
    let workspace = kernel.open_workspace(directory.to_str().unwrap()).unwrap();
    let session = kernel.create_session(&workspace.id).unwrap();
    let backend = TaskBackend::ExternalAgent {
        agent_id: "stable".into(),
    };
    let old = kernel
        .create_task_with_backend(&session.id, "queued", None, backend.clone())
        .unwrap()
        .0;
    drop(kernel);
    let mut kernel = Kernel::open(&database, Box::new(DemoProvider)).unwrap();
    assert_eq!(
        kernel.get_task_resume_status(&old.id).unwrap().disposition,
        TaskResumeDisposition::AgentUnavailable
    );
    register(&mut kernel, after);
    let before = kernel.event_watermark().unwrap();
    assert_eq!(
        kernel.get_task_resume_status(&old.id).unwrap().disposition,
        TaskResumeDisposition::AgentRegistrationChanged
    );
    assert!(kernel.prepare_resume_task(&old.id, false).is_err());
    assert_eq!(kernel.event_watermark().unwrap(), before);
    let interrupted = kernel.get_task(&old.id).unwrap();
    assert!(interrupted.agent_run.is_none());
    assert_eq!(interrupted.status, TaskStatus::Interrupted);
    let fresh = kernel
        .create_task_with_backend(&session.id, "fresh", None, backend)
        .unwrap()
        .0;
    assert_ne!(fresh.backend_authority, old.backend_authority);
    assert!(
        kernel
            .prepare_agent_run(&fresh.id, &CancellationToken::new())
            .unwrap()
            .is_some()
    );
    drop(kernel);
    std::fs::remove_dir_all(directory).unwrap();
}
#[test]
fn acp_process_arguments_and_filesystem_policy_are_part_of_queued_authority() {
    let config = json!({"agents":[{"id":"stable","command":std::env::current_exe().unwrap(),"isolation":"native_scratch","args":[]}]});
    let mut args = config.clone();
    args["agents"][0]["args"] = json!(["changed"]);
    cannot_rebind(config.clone(), args, true);
    let mut policy = config.clone();
    policy["agents"][0]["workspaceAccess"] = json!(true);
    cannot_rebind(config, policy, true);
}
#[test]
fn a2a_endpoint_and_execution_budget_are_part_of_queued_authority() {
    let config = json!({"agents":[{"id":"stable","endpoint":"http://127.0.0.1:1/a2a"}]});
    let mut endpoint = config.clone();
    endpoint["agents"][0]["endpoint"] = json!("http://127.0.0.1:2/a2a");
    cannot_rebind(config.clone(), endpoint, false);
    let mut budget = config.clone();
    budget["agents"][0]["taskTimeoutMs"] = json!(600000);
    cannot_rebind(config.clone(), budget, false);
    let mut transport = config.clone();
    transport["agents"][0]["streaming"] = json!(true);
    cannot_rebind(config, transport, false);
}

#[test]
fn a2a_tenant_is_part_of_the_persisted_registration_binding() {
    let first =
        json!({"agents":[{"id":"stable","endpoint":"http://127.0.0.1:1/a2a","tenant":"team-a"}]});
    let mut second = first.clone();
    second["agents"][0]["tenant"] = json!("team-b");
    cannot_rebind(first, second, false);
}
