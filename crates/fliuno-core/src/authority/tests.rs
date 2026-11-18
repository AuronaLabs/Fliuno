use super::*;
use crate::*;
use fliuno_protocol::*;
use serde_json::json;
use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

struct Directory(PathBuf);
impl Directory {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("fliuno-authority-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }
}
impl Drop for Directory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct Provider;
impl ModelProvider for Provider {
    fn next(
        &self,
        task: &Task,
        _: &[ToolSpec],
        _: &CancellationToken,
    ) -> std::result::Result<ModelTurn, String> {
        Ok(if task
            .messages
            .iter()
            .any(|message| matches!(message, Message::ToolResult { .. }))
        {
            ModelDecision::Complete("observed".into())
        } else {
            ModelDecision::Call(ToolCall {
                id: format!("call_{}", task.id),
                name: "stable".into(),
                arguments: json!({}),
            })
        }
        .into())
    }
}
struct Counter {
    version: u8,
    calls: Arc<AtomicUsize>,
}
impl Tool for Counter {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "stable".into(),
            description: "counter".into(),
            effect: ToolEffect::External,
            input_schema: json!({"type":"object","version":self.version}),
        }
    }
    fn validate(&self, _: &Value) -> std::result::Result<(), String> {
        Ok(())
    }
    fn execute(
        &self,
        _: &ToolContext<'_>,
        _: &ToolCall,
        _: &CancellationToken,
    ) -> std::result::Result<ToolOutput, ToolError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(ToolOutput {
            output: json!({"ok":true}),
            artifacts: vec![],
        })
    }
}
fn registered(path: &std::path::Path, version: u8, calls: &Arc<AtomicUsize>) -> Kernel {
    let mut kernel = Kernel::open(path, Box::new(Provider)).unwrap();
    kernel
        .register_tool(Box::new(Counter {
            version,
            calls: Arc::clone(calls),
        }))
        .unwrap();
    kernel
}
fn session(kernel: &mut Kernel, directory: &Directory) -> Session {
    let workspace = kernel
        .open_workspace(directory.0.to_str().unwrap())
        .unwrap();
    kernel.create_session(&workspace.id).unwrap()
}
fn approve(kernel: &mut Kernel, task: &Task) -> Task {
    kernel
        .respond_approval(
            &task.id,
            &task.pending_approval.as_ref().unwrap().id,
            ApprovalDecision::AllowOnce,
        )
        .unwrap()
}

#[test]
fn canonical_digest_preserves_domains_types_boundaries_and_array_order() {
    assert_eq!(
        authority_digest("x", &json!({"a":1,"b":{"c":2,"d":3}})),
        authority_digest("x", &json!({"b":{"d":3,"c":2},"a":1}))
    );
    for (left, right) in [
        (json!("1"), json!(1)),
        (json!(["ab", "c"]), json!(["a", "bc"])),
        (json!([1, 2]), json!([2, 1])),
        (json!({"a":1}), json!(["a", 1])),
    ] {
        assert_ne!(authority_digest("x", &left), authority_digest("x", &right));
    }
    assert_ne!(
        authority_digest("x", &json!({})),
        authority_digest("y", &json!({}))
    );
}

#[test]
fn same_name_and_arguments_do_not_rebind_persisted_approval_to_a_changed_contract() {
    let directory = Directory::new();
    let database = directory.0.join("kernel.db");
    let calls = Arc::new(AtomicUsize::new(0));
    let mut kernel = registered(&database, 1, &calls);
    let session = session(&mut kernel, &directory);
    let waiting = kernel.start_task(&session.id, "old contract").unwrap();
    drop(kernel);
    let mut kernel = registered(&database, 2, &calls);
    let failed = approve(&mut kernel, &waiting);
    assert_eq!(failed.status, TaskStatus::Failed);
    assert!(failed.output.unwrap().contains("registration changed"));
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert!(
        !kernel
            .list_events(&waiting.id, 0, 100)
            .unwrap()
            .events
            .iter()
            .any(|event| matches!(event.data, EventKind::ToolStarted { .. }))
    );
    let fresh = kernel.start_task(&session.id, "new contract").unwrap();
    drop(kernel);
    let mut kernel = registered(&database, 2, &calls);
    assert_eq!(approve(&mut kernel, &fresh).status, TaskStatus::Completed);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[test]
fn v7_approval_without_binding_remains_readable_but_cannot_authorize_execution() {
    let directory = Directory::new();
    let database = directory.0.join("kernel.db");
    let calls = Arc::new(AtomicUsize::new(0));
    let mut kernel = registered(&database, 1, &calls);
    let session = session(&mut kernel, &directory);
    let waiting = kernel.start_task(&session.id, "legacy approval").unwrap();
    let mut data = serde_json::to_value(&waiting).unwrap();
    data["pendingApproval"]
        .as_object_mut()
        .unwrap()
        .remove("authority");
    data.as_object_mut().unwrap().remove("backendAuthority");
    kernel
        .db
        .execute(
            "UPDATE tasks SET data=?1 WHERE id=?2",
            rusqlite::params![data.to_string(), waiting.id],
        )
        .unwrap();
    kernel.db.pragma_update(None, "user_version", 7).unwrap();
    drop(kernel);
    let mut kernel = registered(&database, 1, &calls);
    assert_eq!(
        kernel.get_task(&waiting.id).unwrap().status,
        TaskStatus::WaitingApproval
    );
    assert_eq!(approve(&mut kernel, &waiting).status, TaskStatus::Failed);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        kernel
            .db
            .pragma_query_value(None, "user_version", |row| row.get::<_, u32>(0))
            .unwrap(),
        16
    );
}

struct Agent(u8);
impl AgentBackend for Agent {
    fn spec(&self) -> AgentSpec {
        AgentSpec {
            id: "stable".into(),
            protocol: "fixture".into(),
            isolation: "fixture".into(),
            workspace_access: false,
        }
    }
    fn authority(&self) -> String {
        authority_digest("test.agent.v1", &json!({"configuration":self.0}))
    }
    fn execute(
        &self,
        _: &AgentRequest,
        _: &dyn AgentObserver,
        _: tokio::sync::mpsc::Receiver<AgentCommand>,
        _: &CancellationToken,
    ) -> AgentOutcome {
        panic!("preparation test must never execute")
    }
}
#[test]
fn queued_agent_task_keeps_original_registration_across_idempotency_and_restart() {
    let directory = Directory::new();
    let database = directory.0.join("kernel.db");
    let mut kernel = Kernel::open(&database, Box::new(DemoProvider)).unwrap();
    kernel.register_agent(Box::new(Agent(1))).unwrap();
    let session = session(&mut kernel, &directory);
    let backend = TaskBackend::ExternalAgent {
        agent_id: "stable".into(),
    };
    let original = kernel
        .create_task_with_backend(&session.id, "queued", Some("submission"), backend.clone())
        .unwrap()
        .0;
    drop(kernel);
    let mut kernel = Kernel::open(&database, Box::new(DemoProvider)).unwrap();
    kernel.register_agent(Box::new(Agent(2))).unwrap();
    let (duplicate, created) = kernel
        .create_task_with_backend(&session.id, "queued", Some("submission"), backend.clone())
        .unwrap();
    assert!(!created);
    assert_eq!(duplicate.backend_authority, original.backend_authority);
    kernel.prepare_resume_task(&original.id, false).unwrap();
    assert!(
        kernel
            .prepare_agent_run(&original.id, &CancellationToken::new())
            .is_err()
    );
    assert!(kernel.get_task(&original.id).unwrap().agent_run.is_none());
    assert_eq!(
        kernel.get_task(&original.id).unwrap().status,
        TaskStatus::Failed
    );
    let fresh = kernel
        .create_task_with_backend(&session.id, "fresh", None, backend)
        .unwrap()
        .0;
    assert_ne!(fresh.backend_authority, original.backend_authority);
    assert!(
        kernel
            .prepare_agent_run(&fresh.id, &CancellationToken::new())
            .unwrap()
            .is_some()
    );
}

#[test]
fn v7_queued_agent_without_binding_cannot_start_even_with_the_same_registration() {
    let directory = Directory::new();
    let database = directory.0.join("kernel.db");
    let mut kernel = Kernel::open(&database, Box::new(DemoProvider)).unwrap();
    kernel.register_agent(Box::new(Agent(1))).unwrap();
    let session = session(&mut kernel, &directory);
    let old = kernel
        .create_task_with_backend(
            &session.id,
            "legacy queued task",
            None,
            TaskBackend::ExternalAgent {
                agent_id: "stable".into(),
            },
        )
        .unwrap()
        .0;
    let mut data = serde_json::to_value(&old).unwrap();
    data.as_object_mut().unwrap().remove("backendAuthority");
    kernel
        .db
        .execute(
            "UPDATE tasks SET data=?1 WHERE id=?2",
            rusqlite::params![data.to_string(), old.id],
        )
        .unwrap();
    kernel.db.pragma_update(None, "user_version", 7).unwrap();
    drop(kernel);
    let mut kernel = Kernel::open(&database, Box::new(DemoProvider)).unwrap();
    kernel.register_agent(Box::new(Agent(1))).unwrap();
    assert!(
        kernel
            .get_task(&old.id)
            .unwrap()
            .backend_authority
            .is_none()
    );
    kernel.prepare_resume_task(&old.id, false).unwrap();
    assert!(
        kernel
            .prepare_agent_run(&old.id, &CancellationToken::new())
            .is_err()
    );
    assert!(kernel.get_task(&old.id).unwrap().agent_run.is_none());
    assert_eq!(kernel.get_task(&old.id).unwrap().status, TaskStatus::Failed);
    assert!(
        !kernel
            .list_events(&old.id, 0, 100)
            .unwrap()
            .events
            .iter()
            .any(|event| matches!(event.data, EventKind::AgentStarted { .. }))
    );
}

#[test]
fn removed_agent_registration_persists_a_known_failure_before_any_run() {
    let directory = Directory::new();
    let database = directory.0.join("kernel.db");
    let mut kernel = Kernel::open(&database, Box::new(DemoProvider)).unwrap();
    kernel.register_agent(Box::new(Agent(1))).unwrap();
    let session = session(&mut kernel, &directory);
    let old = kernel
        .create_task_with_backend(
            &session.id,
            "queued task",
            None,
            TaskBackend::ExternalAgent {
                agent_id: "stable".into(),
            },
        )
        .unwrap()
        .0;
    // Exercise the preparation guard itself. Resuming an interrupted task
    // without its registration is already rejected by prepare_resume_task.
    kernel.agents.clear();
    assert!(
        kernel
            .prepare_agent_run(&old.id, &CancellationToken::new())
            .is_err()
    );
    drop(kernel);
    let kernel = Kernel::open(&database, Box::new(DemoProvider)).unwrap();
    let failed = kernel.get_task(&old.id).unwrap();
    assert_eq!(failed.status, TaskStatus::Failed);
    assert!(failed.agent_run.is_none());
    let events = kernel.list_events(&old.id, 0, 100).unwrap().events;
    assert!(
        events
            .iter()
            .any(|event| matches!(event.data, EventKind::TaskFailed { .. }))
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event.data, EventKind::AgentStarted { .. }))
    );
}
