use super::*;
use serde_json::{Value, json};
use std::fs;

fn setup() -> (Kernel, Session) {
    let mut kernel = Kernel::in_memory(Box::new(DemoProvider)).unwrap();
    let workspace = kernel
        .open_workspace(std::env::current_dir().unwrap().to_str().unwrap())
        .unwrap();
    let session = kernel.create_session(&workspace.id).unwrap();
    (kernel, session)
}

#[test]
fn approved_task_creates_one_artifact_and_replays_events() {
    let (mut kernel, session) = setup();
    let task = kernel.start_task(&session.id, "hello world").unwrap();
    assert_eq!(task.status, TaskStatus::WaitingApproval);
    assert!(kernel.list_artifacts(&task.id).unwrap().is_empty());
    let approval = task.pending_approval.unwrap();
    let done = kernel
        .respond_approval(&task.id, &approval.id, ApprovalDecision::AllowOnce)
        .unwrap();
    assert_eq!(done.status, TaskStatus::Completed);
    let artifacts = kernel.list_artifacts(&task.id).unwrap();
    assert_eq!(artifacts.len(), 1);
    assert_eq!(artifacts[0].content, "hello world");
    assert!(
        kernel
            .respond_approval(&task.id, &approval.id, ApprovalDecision::AllowOnce)
            .is_err()
    );
    assert_eq!(kernel.list_artifacts(&task.id).unwrap().len(), 1);
    let page = kernel.list_events(&task.id, 0, 2).unwrap();
    assert!(page.has_more);
    let rest = kernel.list_events(&task.id, page.next_after, 100).unwrap();
    assert!(!rest.has_more);
    assert_eq!(page.events.len() + rest.events.len(), 9);
    assert!(rest.events.iter().all(|e| e.sequence > page.next_after));
    assert!(matches!(
        rest.events.last().unwrap().data,
        EventKind::TaskCompleted { .. }
    ));
}

#[test]
fn denying_cancelling_and_cross_task_approvals_cannot_write() {
    let (mut kernel, session) = setup();
    let first = kernel.start_task(&session.id, "first").unwrap();
    let second = kernel.start_task(&session.id, "second").unwrap();
    let approval = first.pending_approval.unwrap();
    assert!(
        kernel
            .respond_approval(&second.id, &approval.id, ApprovalDecision::AllowOnce)
            .is_err()
    );
    assert_eq!(
        kernel
            .respond_approval(&first.id, &approval.id, ApprovalDecision::Deny)
            .unwrap()
            .status,
        TaskStatus::Failed
    );
    assert!(kernel.list_artifacts(&first.id).unwrap().is_empty());
    let cancelled = kernel.cancel_task(&second.id).unwrap();
    assert_eq!(cancelled.status, TaskStatus::Cancelled);
    assert_eq!(
        kernel.cancel_task(&second.id).unwrap().status,
        TaskStatus::Cancelled
    );
    assert!(
        kernel
            .respond_approval(
                &second.id,
                &cancelled.pending_approval.map_or("stale".into(), |a| a.id),
                ApprovalDecision::AllowOnce
            )
            .is_err()
    );
    assert!(kernel.list_artifacts(&second.id).unwrap().is_empty());
}

#[test]
fn validates_references_input_and_pagination() {
    let (mut kernel, session) = setup();
    assert!(kernel.create_session("missing").is_err());
    assert!(kernel.start_task("missing", "hello").is_err());
    assert!(kernel.start_task(&session.id, " ").is_err());
    let task = kernel.start_task(&session.id, "test").unwrap();
    assert!(kernel.list_events(&task.id, u64::MAX, 100).is_err());
    assert!(kernel.list_events(&task.id, 0, 0).is_err());
    assert!(kernel.list_events("missing", 0, 100).is_err());
}

struct UnknownToolProvider;
impl ModelProvider for UnknownToolProvider {
    fn next(
        &self,
        _task: &Task,
        _tools: &[ToolSpec],
        _cancellation: &CancellationToken,
    ) -> std::result::Result<ModelTurn, String> {
        Ok(ModelDecision::Call(ToolCall {
            id: "call".into(),
            name: "shell.exec".into(),
            arguments: json!({}),
        })
        .into())
    }
}

#[test]
fn unregistered_tools_fail_closed() {
    let (mut kernel, session) = setup();
    kernel.set_model_provider(Box::new(UnknownToolProvider));
    let task = kernel.start_task(&session.id, "run command").unwrap();
    assert_eq!(task.status, TaskStatus::Failed);
    assert!(kernel.list_artifacts(&task.id).unwrap().is_empty());
}

#[test]
fn waiting_approval_survives_restart_and_database_is_single_owner() {
    let directory = std::env::temp_dir().join(new_id("fliuno-test"));
    fs::create_dir(&directory).unwrap();
    let path = directory.join("kernel.db");
    let mut kernel = Kernel::open(&path, Box::new(DemoProvider)).unwrap();
    assert!(matches!(
        Kernel::open(&path, Box::new(DemoProvider)),
        Err(Error::Conflict(_))
    ));
    let workspace = kernel.open_workspace(directory.to_str().unwrap()).unwrap();
    let session = kernel.create_session(&workspace.id).unwrap();
    let task = kernel.start_task(&session.id, "persisted").unwrap();
    let approval_id = task.pending_approval.unwrap().id;
    let queued = kernel
        .create_task_with_key(&session.id, "queued before migration", None)
        .unwrap()
        .0;
    kernel
        .db
        .execute_batch(
            "DROP INDEX tasks_session; DROP INDEX sessions_workspace;
        DROP INDEX tasks_running; PRAGMA user_version=11;",
        )
        .unwrap();
    drop(kernel);
    let mut kernel = Kernel::open(&path, Box::new(DemoProvider)).unwrap();
    assert_eq!(
        kernel.get_task(&queued.id).unwrap().status,
        TaskStatus::Interrupted
    );
    let page = kernel.list_tasks(&session.id, 0, 1).unwrap();
    assert_eq!(page.items[0].id, task.id);
    assert_eq!(
        kernel
            .list_tasks(&session.id, page.next_after, 1)
            .unwrap()
            .items[0]
            .id,
        queued.id
    );
    assert_eq!(
        kernel.get_task(&task.id).unwrap().status,
        TaskStatus::WaitingApproval
    );
    assert_eq!(
        kernel
            .respond_approval(&task.id, &approval_id, ApprovalDecision::AllowOnce)
            .unwrap()
            .status,
        TaskStatus::Completed
    );
    drop(kernel);
    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn recovery_interrupts_running_tasks_without_reexecuting_tools() {
    let (mut kernel, session) = setup();
    let mut task = kernel.start_task(&session.id, "recover").unwrap();
    // Simulate a crash after the transactional tool commit but before model completion.
    task.pending_approval = None;
    task.status = TaskStatus::Running;
    task.messages.push(Message::ToolResult {
        call_id: "call".into(),
        output: json!({}),
    });
    kernel.commit(&task, &[], &[]).unwrap();
    kernel.recover().unwrap();
    assert_eq!(
        kernel.get_task(&task.id).unwrap().status,
        TaskStatus::Interrupted
    );
    assert_eq!(
        kernel.resume_task(&task.id).unwrap().status,
        TaskStatus::Completed
    );
    assert!(kernel.list_artifacts(&task.id).unwrap().is_empty());
    assert!(kernel.resume_task(&task.id).is_err());
}

struct FlakyModel(std::sync::Arc<std::sync::atomic::AtomicUsize>);
impl ModelProvider for FlakyModel {
    fn next(
        &self,
        _task: &Task,
        _tools: &[ToolSpec],
        _cancellation: &CancellationToken,
    ) -> std::result::Result<ModelTurn, String> {
        if self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
            Err("provider timeout".into())
        } else {
            Ok(ModelDecision::Complete("recovered after explicit retry".into()).into())
        }
    }
}

#[test]
fn uncertain_model_outcomes_need_explicit_retry_consent() {
    let mut kernel = Kernel::in_memory(Box::new(FlakyModel(std::sync::Arc::new(
        std::sync::atomic::AtomicUsize::new(0),
    ))))
    .unwrap();
    let ws = kernel
        .open_workspace(std::env::current_dir().unwrap().to_str().unwrap())
        .unwrap();
    let session = kernel.create_session(&ws.id).unwrap();
    let task = kernel.start_task(&session.id, "test").unwrap();
    assert_eq!(task.status, TaskStatus::Interrupted);
    assert!(task.model_outcome_unknown);
    assert!(kernel.resume_task(&task.id).is_err());
    let done = kernel.resume_task_with_options(&task.id, true).unwrap();
    assert_eq!(done.status, TaskStatus::Completed);
    assert_eq!(done.model_steps, 2);
    assert!(!done.model_outcome_unknown);
    assert!(
        kernel
            .list_events(&task.id, 0, 100)
            .unwrap()
            .events
            .iter()
            .any(|e| matches!(e.data, EventKind::ModelOutcomeUnknown { step: 1 }))
    );
}

struct FixedModelError(ModelError);

impl ModelProvider for FixedModelError {
    fn next(
        &self,
        _: &Task,
        _: &[ToolSpec],
        _: &CancellationToken,
    ) -> std::result::Result<ModelTurn, String> {
        Err("legacy provider method should not be called".into())
    }

    fn next_classified(
        &self,
        _: &Task,
        _: &[ToolSpec],
        _: &CancellationToken,
    ) -> std::result::Result<ModelTurn, ModelError> {
        Err(self.0.clone())
    }
}

#[test]
fn provider_rejection_before_dispatch_is_a_known_failure() {
    let (mut kernel, session) = setup();
    kernel.set_model_provider(Box::new(FixedModelError(ModelError::NotDispatched(
        "request cannot be built".into(),
    ))));
    let failed = kernel
        .start_task(&session.id, "local provider rejection")
        .unwrap();
    assert_eq!(failed.status, TaskStatus::Failed);
    assert!(!failed.model_outcome_unknown);
    assert_eq!(failed.model_steps, 1);
    assert_eq!(
        failed.output.as_deref(),
        Some("model request rejected before dispatch: request cannot be built")
    );
    assert!(
        kernel
            .list_events(&failed.id, 0, 100)
            .unwrap()
            .events
            .iter()
            .all(|event| !matches!(event.data, EventKind::ModelOutcomeUnknown { .. }))
    );
}

#[test]
fn cancellation_before_provider_dispatch_is_known_and_terminal() {
    let (mut kernel, session) = setup();
    kernel.set_model_provider(Box::new(FixedModelError(
        ModelError::CancelledBeforeDispatch,
    )));
    let cancelled = kernel
        .start_task(&session.id, "cancel before provider send")
        .unwrap();
    assert_eq!(cancelled.status, TaskStatus::Cancelled);
    assert!(!cancelled.model_outcome_unknown);
    assert!(cancelled.output.is_none());
    let events = kernel.list_events(&cancelled.id, 0, 100).unwrap().events;
    assert!(matches!(
        events.last().unwrap().data,
        EventKind::TaskCancelled
    ));
    assert!(
        events
            .iter()
            .all(|event| !matches!(event.data, EventKind::ModelOutcomeUnknown { .. }))
    );
}

#[test]
fn model_attempts_are_exclusive_and_stale_responses_cannot_overwrite_a_retry() {
    let (mut kernel, session) = setup();
    let (task, _) = kernel
        .create_task_with_key(&session.id, "attempts", None)
        .unwrap();
    let PreparedModelStep::Ready { task: first, .. } = kernel.prepare_model_step(&task.id).unwrap()
    else {
        panic!("expected a model attempt");
    };
    assert!(matches!(
        kernel.prepare_model_step(&task.id),
        Err(Error::Conflict(_))
    ));
    kernel.interrupt_running_tasks().unwrap();
    kernel.prepare_resume_task(&task.id, true).unwrap();
    let PreparedModelStep::Ready { task: second, .. } =
        kernel.prepare_model_step(&task.id).unwrap()
    else {
        panic!("expected a retry attempt");
    };
    assert!(matches!(
        kernel.apply_model_response(first, Ok(ModelDecision::Complete("stale".into()).into())),
        Err(Error::Conflict(_))
    ));
    let current = kernel.get_task(&task.id).unwrap();
    assert_eq!(current.status, TaskStatus::Running);
    assert_eq!(current.model_steps, 2);
    assert!(current.output.is_none());
    let done = kernel
        .apply_model_response(second, Ok(ModelDecision::Complete("fresh".into()).into()))
        .unwrap();
    assert_eq!(done.status, TaskStatus::Completed);
    assert_eq!(done.output.as_deref(), Some("fresh"));
}

struct ExternalProvider;
impl ModelProvider for ExternalProvider {
    fn next(
        &self,
        _task: &Task,
        _tools: &[ToolSpec],
        _cancellation: &CancellationToken,
    ) -> std::result::Result<ModelTurn, String> {
        Ok(ModelDecision::Call(ToolCall {
            id: "external_call".into(),
            name: "external".into(),
            arguments: json!({}),
        })
        .into())
    }
}

struct UncertainExternal;
impl Tool for UncertainExternal {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "external".into(),
            description: "test external operation".into(),
            effect: ToolEffect::External,
            input_schema: json!({"type":"object"}),
        }
    }
    fn validate(&self, _arguments: &serde_json::Value) -> std::result::Result<(), String> {
        Ok(())
    }
    fn execute(
        &self,
        _context: &ToolContext<'_>,
        _call: &ToolCall,
        _cancellation: &CancellationToken,
    ) -> std::result::Result<ToolOutput, ToolError> {
        Err(ToolError::OutcomeUnknown("connection lost".into()))
    }
}

#[test]
fn unknown_external_effect_is_journaled_and_cannot_be_retried() {
    let (mut kernel, session) = setup();
    kernel.set_model_provider(Box::new(ExternalProvider));
    kernel.register_tool(Box::new(UncertainExternal)).unwrap();
    let task = kernel.start_task(&session.id, "send once").unwrap();
    let id = task.pending_approval.unwrap().id;
    let task = kernel
        .respond_approval(&task.id, &id, ApprovalDecision::AllowOnce)
        .unwrap();
    assert_eq!(task.status, TaskStatus::Interrupted);
    assert_eq!(task.in_flight_tool.unwrap().id, "external_call");
    assert!(kernel.resume_task(&task.id).is_err());
    assert!(
        kernel
            .respond_approval(&task.id, &id, ApprovalDecision::AllowOnce)
            .is_err()
    );
    assert!(
        kernel
            .list_events(&task.id, 0, 100)
            .unwrap()
            .events
            .iter()
            .any(|e| matches!(e.data, EventKind::ToolStarted { .. }))
    );
    assert_eq!(
        kernel.cancel_task(&task.id).unwrap().status,
        TaskStatus::Cancelled
    );
}

struct UncertainWithEvidence;
impl Tool for UncertainWithEvidence {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "uncertain_evidence".into(),
            description: "partial evidence fixture".into(),
            effect: ToolEffect::External,
            input_schema: json!({"type":"object"}),
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
        Err(ToolError::OutcomeUnknownWithOutput {
            message: "timed out with output".into(),
            output: json!({"stdout":{"data":"partial"}}),
        })
    }
}

#[test]
fn uncertain_execution_evidence_is_durable_but_never_replayed_or_contextualized() {
    struct EvidenceProvider;
    impl ModelProvider for EvidenceProvider {
        fn next(
            &self,
            _: &Task,
            _: &[ToolSpec],
            _: &CancellationToken,
        ) -> std::result::Result<ModelTurn, String> {
            Ok(ModelDecision::Call(ToolCall {
                id: "uncertain".into(),
                name: "uncertain_evidence".into(),
                arguments: json!({}),
            })
            .into())
        }
    }
    let (mut kernel, session) = setup();
    kernel.set_model_provider(Box::new(EvidenceProvider));
    kernel
        .register_tool(Box::new(UncertainWithEvidence))
        .unwrap();
    let task = kernel.start_task(&session.id, "partial evidence").unwrap();
    let task = kernel
        .respond_approval(
            &task.id,
            &task.pending_approval.unwrap().id,
            ApprovalDecision::AllowOnce,
        )
        .unwrap();
    assert_eq!(task.status, TaskStatus::Interrupted);
    assert!(task.in_flight_tool.is_some());
    let events = kernel.list_events(&task.id, 0, 100).unwrap().events;
    assert!(events.iter().any(|event|matches!(&event.data,EventKind::ToolOutcomeUnknown{output:Some(output),..} if output["stdout"]["data"]=="partial")));
    assert!(kernel.resume_task(&task.id).is_err());
    assert!(
        kernel
            .session_context(&session.id, Default::default())
            .unwrap()
            .turns
            .is_empty()
    );
}

struct LoopingProvider;
impl ModelProvider for LoopingProvider {
    fn next(
        &self,
        _task: &Task,
        _tools: &[ToolSpec],
        _cancellation: &CancellationToken,
    ) -> std::result::Result<ModelTurn, String> {
        Ok(ModelDecision::Call(ToolCall {
            id: new_id("call"),
            name: "artifact_create".into(),
            arguments: json!({"title":"again","content":"x"}),
        })
        .into())
    }
}

#[test]
fn tool_loops_stop_at_persisted_model_budget() {
    let (mut kernel, session) = setup();
    kernel.set_model_provider(Box::new(LoopingProvider));
    let mut task = kernel.start_task(&session.id, "loop").unwrap();
    for _ in 0..MAX_MODEL_STEPS {
        let id = task.pending_approval.as_ref().unwrap().id.clone();
        task = kernel
            .respond_approval(&task.id, &id, ApprovalDecision::AllowOnce)
            .unwrap();
    }
    assert_eq!(task.status, TaskStatus::Failed);
    assert_eq!(task.model_steps, MAX_MODEL_STEPS);
    assert_eq!(
        kernel.list_artifacts(&task.id).unwrap().len(),
        MAX_MODEL_STEPS as usize
    );
}

struct DenyPolicy;
impl PolicyEngine for DenyPolicy {
    fn evaluate(&self, _spec: &ToolSpec, _call: &ToolCall) -> PolicyDecision {
        PolicyDecision::Deny("policy changed".into())
    }
}

#[test]
fn changed_policy_blocks_old_approval_and_keeps_decision_in_audit() {
    let (mut kernel, session) = setup();
    let task = kernel.start_task(&session.id, "test").unwrap();
    kernel.set_policy(Box::new(DenyPolicy));
    let done = kernel
        .respond_approval(
            &task.id,
            &task.pending_approval.unwrap().id,
            ApprovalDecision::AllowOnce,
        )
        .unwrap();
    assert_eq!(done.status, TaskStatus::Failed);
    assert!(kernel.list_artifacts(&task.id).unwrap().is_empty());
    assert!(
        kernel
            .list_events(&task.id, 0, 100)
            .unwrap()
            .events
            .iter()
            .any(|e| matches!(
                e.data,
                EventKind::ApprovalResolved {
                    decision: ApprovalDecision::AllowOnce,
                    ..
                }
            ))
    );
}

#[test]
fn lost_start_responses_are_safe_to_retry_with_the_same_key() {
    let (mut kernel, session) = setup();
    let task = kernel
        .start_task_with_key(&session.id, "once", Some("client-key"))
        .unwrap();
    assert_eq!(
        kernel
            .start_task_with_key(&session.id, "once", Some("client-key"))
            .unwrap()
            .id,
        task.id
    );
    assert!(
        kernel
            .start_task_with_key(&session.id, "different", Some("client-key"))
            .is_err()
    );
    kernel
        .respond_approval(
            &task.id,
            &task.pending_approval.unwrap().id,
            ApprovalDecision::AllowOnce,
        )
        .unwrap();
    let retry = kernel
        .start_task_with_key(&session.id, "once", Some("client-key"))
        .unwrap();
    assert_eq!(retry.status, TaskStatus::Completed);
    assert_eq!(retry.id, task.id);
    assert_eq!(
        kernel.list_tasks(&session.id, 0, 100).unwrap().items.len(),
        1
    );
    assert_eq!(kernel.list_artifacts(&task.id).unwrap().len(), 1);
}

#[test]
fn object_lists_are_scoped_and_paginated() {
    let (mut kernel, session) = setup();
    let workspace = kernel.list_workspaces(0, 100).unwrap().items.remove(0);
    assert_eq!(
        kernel.list_sessions(&workspace.id, 0, 100).unwrap().items[0].id,
        session.id
    );
    kernel.start_task(&session.id, "one").unwrap();
    kernel.start_task(&session.id, "two").unwrap();
    let first = kernel.list_tasks(&session.id, 0, 1).unwrap();
    assert!(first.has_more);
    let second = kernel.list_tasks(&session.id, first.next_after, 1).unwrap();
    assert!(!second.has_more);
    assert_ne!(first.items[0].id, second.items[0].id);
    let other = kernel.create_session(&workspace.id).unwrap();
    assert!(
        kernel
            .list_tasks(&other.id, 0, 100)
            .unwrap()
            .items
            .is_empty()
    );
    assert!(kernel.list_tasks(&session.id, 0, 101).is_err());
    assert!(kernel.list_sessions("missing", 0, 100).is_err());
}

#[test]
fn committed_events_fan_out_and_global_cursor_replays_without_gaps() {
    let (mut kernel, session) = setup();
    let receiver = kernel.subscribe_events().unwrap();
    let first = kernel
        .start_task(&session.id, "first event stream")
        .unwrap();
    let second = kernel
        .start_task(&session.id, "second event stream")
        .unwrap();
    let live: Vec<_> = receiver.try_iter().collect();
    assert_eq!(live.len(), 6);
    assert!(
        live.windows(2)
            .all(|pair| pair[0].sequence < pair[1].sequence)
    );
    assert!(
        live.iter()
            .all(|event| event.task_id == first.id || event.task_id == second.id)
    );

    let mut replayed = Vec::new();
    let mut cursor = 0;
    loop {
        let page = kernel.list_all_events(cursor, 2).unwrap();
        replayed.extend(page.events);
        cursor = page.next_after;
        if !page.has_more {
            break;
        }
    }
    assert_eq!(
        serde_json::to_value(replayed).unwrap(),
        serde_json::to_value(live).unwrap()
    );
}
