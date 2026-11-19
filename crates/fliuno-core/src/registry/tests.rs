//! A late failure must preserve the complete existing registry and pending
//! approvals; registration itself cannot execute any staged tool.
use super::*;
use crate::{
    CancellationToken, DemoProvider, ModelDecision, ModelProvider, ModelTurn, ToolContext,
    ToolEffect, ToolError, ToolOutput, authority_digest,
};
use fliuno_protocol::{ApprovalDecision, Message, Task, TaskStatus, ToolCall};
use serde_json::{Value, json};
use std::sync::atomic::{AtomicUsize, Ordering};

struct Probe {
    name: String,
    identity: String,
    calls: Arc<AtomicUsize>,
    drops: Arc<AtomicUsize>,
}
impl Tool for Probe {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.name.clone(),
            description: "Registration probe".into(),
            effect: ToolEffect::External,
            input_schema: json!({"type":"object","additionalProperties":false}),
        }
    }
    fn authority(&self) -> String {
        self.identity.clone()
    }
    fn validate(&self, arguments: &Value) -> std::result::Result<(), String> {
        if arguments != &json!({}) {
            return Err("probe takes no arguments".into());
        }
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
            output: json!({"tool":self.name}),
            artifacts: vec![],
        })
    }
}
impl Drop for Probe {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
    }
}
fn probe(name: &str, calls: &Arc<AtomicUsize>, drops: &Arc<AtomicUsize>) -> Box<dyn Tool> {
    Box::new(Probe {
        name: name.into(),
        identity: authority_digest("registry.probe.v1", &json!({"name":name})),
        calls: Arc::clone(calls),
        drops: Arc::clone(drops),
    })
}
fn snapshot(kernel: &Kernel) -> Vec<Value> {
    kernel
        .tool_specs()
        .into_iter()
        .map(|spec| {
            json!({"name":spec.name,"description":spec.description,"effect":spec.effect,
                "inputSchema":spec.input_schema,"authority":kernel.tools[&spec.name].authority()})
        })
        .collect()
}

#[test]
fn late_name_authority_and_duplicate_failures_preserve_every_existing_tool_and_drop_staging() {
    let calls = Arc::new(AtomicUsize::new(0));
    let drops = Arc::new(AtomicUsize::new(0));
    let mut kernel = Kernel::in_memory(Box::new(DemoProvider)).unwrap();
    kernel.register_tool(probe("kept", &calls, &drops)).unwrap();
    let before = snapshot(&kernel);
    let valid_identity = "0".repeat(64);
    let cases = [
        ("".into(), valid_identity.clone()),
        ("bad name".into(), valid_identity.clone()),
        ("bad.name".into(), valid_identity.clone()),
        ("a".repeat(65), valid_identity.clone()),
        ("中文".into(), valid_identity.clone()),
        ("kept".into(), valid_identity.clone()),
        ("artifact_create".into(), valid_identity.clone()),
        ("fresh".into(), valid_identity),
        ("invalid_identity".into(), "0".repeat(63)),
        ("invalid_identity".into(), "A".repeat(64)),
        ("invalid_identity".into(), "g".repeat(64)),
    ];
    for (index, (name, identity)) in cases.into_iter().enumerate() {
        let bad: Box<dyn Tool> = Box::new(Probe {
            name,
            identity,
            calls: Arc::clone(&calls),
            drops: Arc::clone(&drops),
        });
        assert!(matches!(
            kernel.register_tools(vec![
                probe("fresh", &calls, &drops),
                bad,
                probe("unvisited_tail", &calls, &drops),
            ]),
            Err(Error::Conflict(_))
        ));
        assert_eq!(snapshot(&kernel), before);
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert_eq!(drops.load(Ordering::SeqCst), (index + 1) * 3);
    }
}

#[test]
fn successful_and_empty_batches_publish_complete_sorted_sets_without_executing_them() {
    let calls = Arc::new(AtomicUsize::new(0));
    let drops = Arc::new(AtomicUsize::new(0));
    let mut kernel = Kernel::in_memory(Box::new(DemoProvider)).unwrap();
    let before = snapshot(&kernel);
    kernel.register_tools(vec![]).unwrap();
    assert_eq!(snapshot(&kernel), before);
    let longest = "a".repeat(64);
    kernel
        .register_tools(vec![
            probe("zeta-1", &calls, &drops),
            probe("Alpha_2", &calls, &drops),
            probe(&longest, &calls, &drops),
        ])
        .unwrap();
    assert_eq!(
        kernel
            .tool_specs()
            .into_iter()
            .map(|spec| spec.name)
            .collect::<Vec<_>>(),
        vec![
            "Alpha_2".to_owned(),
            longest,
            "artifact_create".to_owned(),
            "artifact_create_binary".to_owned(),
            "zeta-1".to_owned()
        ]
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(drops.load(Ordering::SeqCst), 0);
    drop(kernel);
    assert_eq!(drops.load(Ordering::SeqCst), 3);
}

struct Provider;
impl ModelProvider for Provider {
    fn next(
        &self,
        task: &Task,
        _: &[ToolSpec],
        _: &CancellationToken,
    ) -> std::result::Result<ModelTurn, String> {
        if task
            .messages
            .iter()
            .any(|message| matches!(message, Message::ToolResult { .. }))
        {
            return Ok(ModelDecision::Complete("observed".into()).into());
        }
        let Message::User { text } = &task.messages[0] else {
            unreachable!()
        };
        Ok(ModelDecision::Call(ToolCall {
            id: format!("call_{}", task.id),
            name: text.clone(),
            arguments: json!({}),
        })
        .into())
    }
}

#[test]
fn batch_tools_still_need_fresh_approval_and_failed_registration_preserves_a_pending_call() {
    let calls = Arc::new(AtomicUsize::new(0));
    let drops = Arc::new(AtomicUsize::new(0));
    let mut kernel = Kernel::in_memory(Box::new(Provider)).unwrap();
    kernel
        .register_tools(vec![
            probe("batch_one", &calls, &drops),
            probe("batch_two", &calls, &drops),
        ])
        .unwrap();
    let workspace = kernel
        .open_workspace(std::env::current_dir().unwrap().to_str().unwrap())
        .unwrap();
    let session = kernel.create_session(&workspace.id).unwrap();
    let denied = kernel.start_task(&session.id, "batch_one").unwrap();
    assert_eq!(denied.status, TaskStatus::WaitingApproval);
    let denied = kernel
        .respond_approval(
            &denied.id,
            &denied.pending_approval.as_ref().unwrap().id,
            ApprovalDecision::Deny,
        )
        .unwrap();
    assert_eq!(denied.status, TaskStatus::Failed);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    let waiting = kernel.start_task(&session.id, "batch_two").unwrap();
    let approval = waiting.pending_approval.as_ref().unwrap();
    assert_eq!(
        approval.authority.as_ref().unwrap(),
        &kernel.tools["batch_two"].authority()
    );
    assert!(
        kernel
            .register_tools(vec![
                probe("unpublished", &calls, &drops),
                probe("batch_two", &calls, &drops),
            ])
            .is_err()
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    let done = kernel
        .respond_approval(&waiting.id, &approval.id, ApprovalDecision::AllowOnce)
        .unwrap();
    assert_eq!(done.status, TaskStatus::Completed);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert!(done.messages.iter().any(|message| matches!(message, Message::ToolResult { output, .. } if output == &json!({"tool":"batch_two"}))));
    assert!(
        kernel
            .respond_approval(&waiting.id, &approval.id, ApprovalDecision::AllowOnce)
            .is_err()
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}
