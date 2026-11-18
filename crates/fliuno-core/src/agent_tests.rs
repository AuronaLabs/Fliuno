use super::*;
#[path = "agent_remote_tests.rs"]
mod remote;
use serde_json::json;
use std::time::{SystemTime, UNIX_EPOCH};

struct Fixture;
impl AgentBackend for Fixture {
    fn supports_remote_continuation(&self) -> bool {
        true
    }
    fn spec(&self) -> AgentSpec {
        AgentSpec {
            id: "fixture".into(),
            protocol: "test".into(),
            isolation: "test".into(),
            workspace_access: false,
        }
    }
    fn execute(
        &self,
        _: &AgentRequest,
        _: &dyn AgentObserver,
        _: tokio::sync::mpsc::Receiver<AgentCommand>,
        _: &CancellationToken,
    ) -> AgentOutcome {
        panic!("state-machine tests must not execute a backend")
    }
}

fn setup() -> (Kernel, Session) {
    let mut kernel = Kernel::in_memory(Box::new(DemoProvider)).unwrap();
    kernel.register_agent(Box::new(Fixture)).unwrap();
    let workspace = kernel
        .open_workspace(std::env::current_dir().unwrap().to_str().unwrap())
        .unwrap();
    let session = kernel.create_session(&workspace.id).unwrap();
    (kernel, session)
}

fn backend() -> TaskBackend {
    TaskBackend::ExternalAgent {
        agent_id: "fixture".into(),
    }
}
fn start(kernel: &mut Kernel, session: &Session) -> (Task, PreparedAgentRun) {
    let task = kernel
        .create_task_with_backend(&session.id, "test", None, backend())
        .unwrap()
        .0;
    let prepared = kernel
        .prepare_agent_run(&task.id, &CancellationToken::new())
        .unwrap()
        .unwrap();
    kernel
        .observe_agent(
            &task.id,
            &prepared.request.run_id,
            AgentObservation::SessionOpened("external-session".into()),
        )
        .unwrap();
    (task, prepared)
}
fn permission(
    kernel: &mut Kernel,
    task: &Task,
    run: &PreparedAgentRun,
    id: &str,
    allow: bool,
) -> AgentApproval {
    let expiry = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
        + 60_000;
    kernel
        .observe_agent(
            &task.id,
            &run.request.run_id,
            AgentObservation::PermissionRequested {
                permission_id: id.into(),
                request: json!({"operation":"read-only-display"}),
                allow_once_available: allow,
                expires_at_unix_ms: expiry,
            },
        )
        .unwrap();
    kernel
        .get_task(&task.id)
        .unwrap()
        .agent_run
        .unwrap()
        .pending_approvals
        .last()
        .unwrap()
        .clone()
}

#[test]
fn agent_identity_is_part_of_idempotency_and_cannot_enter_model_path() {
    let (mut kernel, session) = setup();
    let first = kernel
        .create_task_with_backend(&session.id, "hello", Some("key"), backend())
        .unwrap()
        .0;
    let (duplicate, created) = kernel
        .create_task_with_backend(&session.id, "hello", Some("key"), backend())
        .unwrap();
    assert!(!created);
    assert_eq!(duplicate.id, first.id);
    assert!(
        kernel
            .create_task_with_key(&session.id, "hello", Some("key"))
            .is_err()
    );
    assert!(kernel.prepare_model_step(&first.id).is_err());
    assert!(
        kernel
            .apply_model_response(
                first.clone(),
                Ok(ModelDecision::Complete("wrong".into()).into())
            )
            .is_err()
    );
    assert!(
        kernel
            .create_task_with_backend(
                &session.id,
                "x",
                None,
                TaskBackend::ExternalAgent {
                    agent_id: "missing".into()
                }
            )
            .is_err()
    );
    assert!(kernel.get_task(&first.id).unwrap().agent_run.is_none());
}

#[test]
fn concurrent_permissions_are_bound_once_and_deny_does_not_fail_agent() {
    let (mut kernel, session) = setup();
    let (task, mut prepared) = start(&mut kernel, &session);
    let a = permission(&mut kernel, &task, &prepared, "one", true);
    let b = permission(&mut kernel, &task, &prepared, "two", false);
    let (other, _) = start(&mut kernel, &session);
    assert!(
        kernel
            .respond_agent_approval(&other.id, &a.id, ApprovalDecision::AllowOnce)
            .is_err()
    );
    assert!(
        kernel
            .respond_agent_approval(&task.id, &b.id, ApprovalDecision::AllowOnce)
            .is_err()
    );
    let waiting = kernel
        .respond_agent_approval(&task.id, &a.id, ApprovalDecision::AllowOnce)
        .unwrap();
    assert_eq!(waiting.status, TaskStatus::WaitingApproval);
    assert_eq!(waiting.agent_run.unwrap().pending_approvals.len(), 1);
    let AgentCommand::Permission {
        permission_id,
        decision,
    } = prepared.commands.try_recv().unwrap()
    else {
        panic!("expected permission");
    };
    assert_eq!(permission_id, "one");
    assert_eq!(decision, ApprovalDecision::AllowOnce);
    assert!(
        kernel
            .respond_agent_approval(&task.id, &a.id, ApprovalDecision::AllowOnce)
            .is_err()
    );
    assert_eq!(
        kernel
            .respond_agent_approval(&task.id, &b.id, ApprovalDecision::Deny)
            .unwrap()
            .status,
        TaskStatus::Running
    );
    assert!(matches!(
        prepared.commands.try_recv().unwrap(),
        AgentCommand::Permission {
            decision: ApprovalDecision::Deny,
            ..
        }
    ));
    let events = kernel.list_events(&task.id, 0, 100).unwrap();
    assert_eq!(
        events
            .events
            .iter()
            .filter(|e| matches!(e.data, EventKind::AgentApprovalResolved { .. }))
            .count(),
        2
    );
}

#[test]
fn consent_is_committed_before_failed_delivery_and_never_replayed() {
    let (mut kernel, session) = setup();
    let (task, prepared) = start(&mut kernel, &session);
    let run_id = prepared.request.run_id.clone();
    let approval = permission(&mut kernel, &task, &prepared, "one", true);
    drop(prepared); // Worker disappeared before consuming the durable decision.
    let result = kernel
        .respond_agent_approval(&task.id, &approval.id, ApprovalDecision::AllowOnce)
        .unwrap();
    assert_eq!(result.status, TaskStatus::Interrupted);
    assert!(result.agent_run.unwrap().outcome_unknown);
    assert!(kernel.prepare_resume_task(&task.id, true).is_err());
    assert!(
        kernel
            .respond_agent_approval(&task.id, &approval.id, ApprovalDecision::AllowOnce)
            .is_err()
    );
    let events = kernel.list_events(&task.id, 0, 100).unwrap().events;
    let resolved = events
        .iter()
        .position(|e| matches!(e.data, EventKind::AgentApprovalResolved { .. }))
        .unwrap();
    let interrupted = events
        .iter()
        .position(|e| matches!(e.data, EventKind::TaskInterrupted))
        .unwrap();
    assert!(resolved < interrupted);
    let late = kernel
        .finish_agent_run(
            &task.id,
            &run_id,
            AgentOutcome {
                stop_reason: Some(AgentStopReason::EndTurn),
                outcome_unknown: false,
            },
        )
        .unwrap();
    assert_eq!(late.status, TaskStatus::Interrupted);
    assert!(late.agent_run.unwrap().outcome_unknown);
}

#[test]
fn failed_consent_transaction_never_delivers_authority() {
    let (mut kernel, session) = setup();
    let (task, mut run) = start(&mut kernel, &session);
    let approval = permission(&mut kernel, &task, &run, "one", true);
    kernel.db.execute_batch("CREATE TRIGGER reject_consent BEFORE INSERT ON events WHEN json_extract(NEW.data,'$.type')='agent_approval_resolved' BEGIN SELECT RAISE(ABORT,'fixture storage failure'); END;").unwrap();
    assert!(
        kernel
            .respond_agent_approval(&task.id, &approval.id, ApprovalDecision::AllowOnce)
            .is_err()
    );
    assert!(run.commands.try_recv().is_err());
    let saved = kernel.get_task(&task.id).unwrap();
    assert_eq!(saved.status, TaskStatus::WaitingApproval);
    assert_eq!(
        saved.agent_run.unwrap().pending_approvals[0].id,
        approval.id
    );
}

#[test]
fn only_never_started_agent_tasks_can_be_resumed_and_need_a_registered_backend() {
    let (mut kernel, session) = setup();
    let task = kernel
        .create_task_with_backend(&session.id, "queued", None, backend())
        .unwrap()
        .0;
    kernel.interrupt_running_tasks().unwrap();
    assert!(kernel.resume_task(&task.id).is_err());
    assert_eq!(
        kernel.get_task(&task.id).unwrap().status,
        TaskStatus::Interrupted
    );
    kernel.agents.clear(); // Simulate restart without the previous host config.
    assert!(kernel.prepare_resume_task(&task.id, true).is_err());
    assert_eq!(
        kernel.get_task(&task.id).unwrap().status,
        TaskStatus::Interrupted
    );
    kernel.register_agent(Box::new(Fixture)).unwrap();
    assert_eq!(
        kernel.prepare_resume_task(&task.id, false).unwrap().status,
        TaskStatus::Running
    );
    assert!(
        kernel
            .prepare_agent_run(&task.id, &CancellationToken::new())
            .unwrap()
            .is_some()
    );
}

#[test]
fn recovery_finalizes_cancelled_runs_without_changing_the_terminal_state() {
    let (mut kernel, session) = setup();
    let (task, _run) = start(&mut kernel, &session);
    kernel.cancel_task(&task.id).unwrap();
    kernel.interrupt_running_tasks().unwrap();
    let task = kernel.get_task(&task.id).unwrap();
    assert_eq!(task.status, TaskStatus::Cancelled);
    let run = task.agent_run.unwrap();
    assert!(run.finished && run.outcome_unknown);
    let count = kernel.list_events(&task.id, 0, 100).unwrap().events.len();
    kernel.interrupt_running_tasks().unwrap();
    assert_eq!(
        kernel.list_events(&task.id, 0, 100).unwrap().events.len(),
        count
    );
}

#[test]
fn version_two_snapshots_upgrade_with_model_defaults_and_preserve_the_event_log() {
    let (mut kernel, session) = setup();
    let task = kernel
        .create_task_with_key(&session.id, "legacy", None)
        .unwrap()
        .0;
    let mut snapshot = serde_json::to_value(&task).unwrap();
    let object = snapshot.as_object_mut().unwrap();
    object.remove("backend");
    object.remove("agentRun");
    kernel
        .db
        .execute(
            "UPDATE tasks SET data=?1 WHERE id=?2",
            rusqlite::params![snapshot.to_string(), task.id],
        )
        .unwrap();
    kernel.db.pragma_update(None, "user_version", 2).unwrap();
    let db = std::mem::replace(
        &mut kernel.db,
        rusqlite::Connection::open_in_memory().unwrap(),
    );
    drop(kernel);
    let recovered = Kernel::from_connection(db, Box::new(DemoProvider), None).unwrap();
    assert_eq!(
        recovered
            .db
            .pragma_query_value(None, "user_version", |r| r.get::<_, u32>(0))
            .unwrap(),
        12
    );
    let restored = recovered.get_task(&task.id).unwrap();
    assert_eq!(restored.backend, TaskBackend::Model);
    assert!(restored.agent_run.is_none());
    assert_eq!(restored.status, TaskStatus::Interrupted);
    let events = recovered.list_events(&task.id, 0, 100).unwrap().events;
    assert!(matches!(events[0].data, EventKind::TaskStarted));
    assert!(matches!(events[1].data, EventKind::TaskInterrupted));
}

#[test]
fn version_three_agent_snapshots_default_host_fields_and_expire_legacy_permissions() {
    let (mut kernel, session) = setup();
    let (task, run) = start(&mut kernel, &session);
    let approval = permission(&mut kernel, &task, &run, "legacy", true);
    let mut snapshot = serde_json::to_value(kernel.get_task(&task.id).unwrap()).unwrap();
    let agent_run = snapshot["agentRun"].as_object_mut().unwrap();
    agent_run.remove("hostTools");
    agent_run.get_mut("pendingApprovals").unwrap()[0]
        .as_object_mut()
        .unwrap()
        .remove("source");
    kernel
        .db
        .execute(
            "UPDATE tasks SET data=?1 WHERE id=?2",
            rusqlite::params![snapshot.to_string(), task.id],
        )
        .unwrap();
    // Older events must remain readable too, not just the task snapshot.
    kernel.db.execute(
        "UPDATE events SET data=json_remove(data,'$.approval.source') WHERE task_id=?1 AND json_extract(data,'$.type')='agent_approval_requested'",
        [&task.id],
    ).unwrap();
    kernel.db.pragma_update(None, "user_version", 3).unwrap();
    let db = std::mem::replace(
        &mut kernel.db,
        rusqlite::Connection::open_in_memory().unwrap(),
    );
    drop(kernel);
    let mut recovered = Kernel::from_connection(db, Box::new(DemoProvider), None).unwrap();
    assert_eq!(
        recovered
            .db
            .pragma_query_value(None, "user_version", |r| r.get::<_, u32>(0))
            .unwrap(),
        12
    );
    let task = recovered.get_task(&task.id).unwrap();
    assert_eq!(task.status, TaskStatus::Interrupted);
    let recorded = task.agent_run.unwrap();
    assert!(recorded.finished && recorded.outcome_unknown);
    assert!(recorded.host_tools.is_empty() && recorded.pending_approvals.is_empty());
    let events = recovered.list_events(&task.id, 0, 100).unwrap().events;
    assert!(events.iter().any(|event| matches!(
        &event.data,
        EventKind::AgentApprovalRequested { approval }
            if approval.source == AgentApprovalSource::AgentPermission
    )));
    assert!(events.iter().any(|event| matches!(
        &event.data,
        EventKind::AgentApprovalClosed { approval_id, .. } if approval_id == &approval.id
    )));
    assert!(
        recovered
            .respond_agent_approval(&task.id, &approval.id, ApprovalDecision::AllowOnce)
            .is_err()
    );
    assert!(recovered.prepare_resume_task(&task.id, true).is_err());
}

struct HostFileTool;
impl Tool for HostFileTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "host_test_file".into(),
            description: "host fixture".into(),
            effect: ToolEffect::WorkspaceWrite,
            input_schema: json!({"type":"object"}),
        }
    }
    fn validate(&self, args: &serde_json::Value) -> std::result::Result<(), String> {
        if args == &json!({"path":"bound.txt"}) {
            Ok(())
        } else {
            Err("immutable args required".into())
        }
    }
    fn execute(
        &self,
        _: &ToolContext<'_>,
        _: &ToolCall,
        _: &CancellationToken,
    ) -> std::result::Result<ToolOutput, ToolError> {
        Ok(ToolOutput {
            output: json!({"written":true}),
            artifacts: vec![],
        })
    }
}
fn host_request(
    kernel: &mut Kernel,
    task: &Task,
    run: &PreparedAgentRun,
    id: &str,
) -> AgentApproval {
    kernel
        .observe_agent_tool(
            &task.id,
            &run.request.run_id,
            AgentToolObservation::Requested {
                request_id: id.into(),
                call: ToolCall {
                    id: id.into(),
                    name: "host_test_file".into(),
                    arguments: json!({"path":"bound.txt"}),
                },
                preview: json!({"diff":"reviewed"}),
                expires_at_unix_ms: SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_millis() as u64
                    + 60_000,
            },
        )
        .unwrap();
    kernel
        .get_task(&task.id)
        .unwrap()
        .agent_run
        .unwrap()
        .pending_approvals
        .last()
        .unwrap()
        .clone()
}

#[test]
fn host_tools_need_independent_consent_and_a_durable_start_before_results() {
    let (mut kernel, session) = setup();
    kernel.register_tool(Box::new(HostFileTool)).unwrap();
    let (task, mut run) = start(&mut kernel, &session);
    let permission = permission(&mut kernel, &task, &run, "agent-grant", true);
    kernel
        .respond_agent_approval(&task.id, &permission.id, ApprovalDecision::AllowOnce)
        .unwrap();
    assert!(matches!(
        run.commands.try_recv().unwrap(),
        AgentCommand::Permission { .. }
    ));
    let approval = host_request(&mut kernel, &task, &run, "host-1");
    assert_eq!(approval.source, AgentApprovalSource::HostTool);
    assert!(
        kernel
            .observe_agent_tool(
                &task.id,
                &run.request.run_id,
                AgentToolObservation::Started {
                    request_id: "host-1".into()
                }
            )
            .is_err()
    );
    kernel
        .respond_agent_approval(&task.id, &approval.id, ApprovalDecision::AllowOnce)
        .unwrap();
    let AgentCommand::HostTool {
        request_id,
        execution: Some(execution),
    } = run.commands.try_recv().unwrap()
    else {
        panic!("expected host execution");
    };
    assert_eq!(execution.call().arguments, json!({"path":"bound.txt"}));
    kernel
        .observe_agent_tool(
            &task.id,
            &run.request.run_id,
            AgentToolObservation::Started {
                request_id: request_id.clone(),
            },
        )
        .unwrap();
    assert!(
        kernel
            .observe_agent_tool(
                &task.id,
                &run.request.run_id,
                AgentToolObservation::Started {
                    request_id: request_id.clone()
                }
            )
            .is_err()
    );
    let result = execution.execute(&CancellationToken::new());
    kernel
        .observe_agent_tool(
            &task.id,
            &run.request.run_id,
            AgentToolObservation::Completed { request_id, result },
        )
        .unwrap();
    let done = kernel
        .finish_agent_run(
            &task.id,
            &run.request.run_id,
            AgentOutcome {
                stop_reason: Some(AgentStopReason::EndTurn),
                outcome_unknown: false,
            },
        )
        .unwrap();
    assert_eq!(done.status, TaskStatus::Completed);
    assert_eq!(
        done.agent_run.unwrap().host_tools[0].status,
        AgentHostToolStatus::Succeeded
    );
    let types = kernel.list_events(&task.id, 0, 100).unwrap().events;
    let began = types
        .iter()
        .position(|e| matches!(e.data, EventKind::AgentHostToolStarted { .. }))
        .unwrap();
    let completed = types
        .iter()
        .position(|e| matches!(e.data, EventKind::AgentHostToolCompleted { .. }))
        .unwrap();
    assert!(began < completed);
}

#[test]
fn restart_or_unknown_host_effect_cannot_be_hidden_by_an_agent_completion() {
    let (mut kernel, session) = setup();
    kernel.register_tool(Box::new(HostFileTool)).unwrap();
    let (task, mut run) = start(&mut kernel, &session);
    let approval = host_request(&mut kernel, &task, &run, "host-1");
    kernel
        .respond_agent_approval(&task.id, &approval.id, ApprovalDecision::AllowOnce)
        .unwrap();
    let _execution = run.commands.try_recv().unwrap();
    kernel
        .observe_agent_tool(
            &task.id,
            &run.request.run_id,
            AgentToolObservation::Started {
                request_id: "host-1".into(),
            },
        )
        .unwrap();
    assert!(
        kernel
            .observe_agent_tool(
                &task.id,
                &run.request.run_id,
                AgentToolObservation::Completed {
                    request_id: "host-1".into(),
                    result: Err(ToolError::OutcomeUnknown("write partially applied".into()))
                }
            )
            .is_err()
    );
    let done = kernel
        .finish_agent_run(
            &task.id,
            &run.request.run_id,
            AgentOutcome {
                stop_reason: Some(AgentStopReason::EndTurn),
                outcome_unknown: false,
            },
        )
        .unwrap();
    assert_eq!(done.status, TaskStatus::Interrupted);
    let recorded = done.agent_run.unwrap();
    assert!(recorded.outcome_unknown);
    assert_eq!(
        recorded.host_tools[0].status,
        AgentHostToolStatus::OutcomeUnknown
    );
    let (other, mut other_run) = start(&mut kernel, &session);
    let approval = host_request(&mut kernel, &other, &other_run, "host-2");
    kernel
        .respond_agent_approval(&other.id, &approval.id, ApprovalDecision::AllowOnce)
        .unwrap();
    let _execution = other_run.commands.try_recv().unwrap();
    kernel
        .observe_agent_tool(
            &other.id,
            &other_run.request.run_id,
            AgentToolObservation::Started {
                request_id: "host-2".into(),
            },
        )
        .unwrap();
    kernel.interrupt_running_tasks().unwrap();
    let recovered = kernel.get_task(&other.id).unwrap();
    assert_eq!(
        recovered.agent_run.unwrap().host_tools[0].status,
        AgentHostToolStatus::OutcomeUnknown
    );
    assert!(kernel.prepare_resume_task(&other.id, true).is_err());
}

#[test]
fn cancelled_or_uncommitted_host_consent_cannot_start_execution() {
    let (mut kernel, session) = setup();
    kernel.register_tool(Box::new(HostFileTool)).unwrap();
    let (task, mut run) = start(&mut kernel, &session);
    let approval = host_request(&mut kernel, &task, &run, "host-1");
    kernel.db.execute_batch("CREATE TRIGGER reject_host_consent BEFORE INSERT ON events WHEN json_extract(NEW.data,'$.type')='agent_approval_resolved' BEGIN SELECT RAISE(ABORT,'storage fixture'); END;").unwrap();
    assert!(
        kernel
            .respond_agent_approval(&task.id, &approval.id, ApprovalDecision::AllowOnce)
            .is_err()
    );
    assert!(run.commands.try_recv().is_err());
    assert_eq!(
        kernel
            .get_task(&task.id)
            .unwrap()
            .agent_run
            .unwrap()
            .host_tools[0]
            .status,
        AgentHostToolStatus::AwaitingApproval
    );
    kernel
        .db
        .execute_batch("DROP TRIGGER reject_host_consent;")
        .unwrap();
    kernel
        .respond_agent_approval(&task.id, &approval.id, ApprovalDecision::AllowOnce)
        .unwrap();
    let _execution = run.commands.try_recv().unwrap();
    kernel.cancel_task(&task.id).unwrap();
    assert!(
        kernel
            .observe_agent_tool(
                &task.id,
                &run.request.run_id,
                AgentToolObservation::Started {
                    request_id: "host-1".into()
                }
            )
            .is_err()
    );
    let done = kernel
        .finish_agent_run(
            &task.id,
            &run.request.run_id,
            AgentOutcome {
                stop_reason: Some(AgentStopReason::Cancelled),
                outcome_unknown: false,
            },
        )
        .unwrap();
    assert_eq!(done.status, TaskStatus::Cancelled);
    assert_eq!(
        done.agent_run.unwrap().host_tools[0].status,
        AgentHostToolStatus::Closed
    );
}

#[test]
fn restart_expires_waiting_permissions_and_preserves_tool_approval_semantics() {
    let (mut kernel, session) = setup();
    let (agent, run) = start(&mut kernel, &session);
    let approval = permission(&mut kernel, &agent, &run, "one", true);
    let tool = kernel.start_task(&session.id, "artifact").unwrap();
    // Re-open the same SQLite connection through startup recovery. No external
    // process or permission sender survives the reconstructed Kernel.
    let db = std::mem::replace(
        &mut kernel.db,
        rusqlite::Connection::open_in_memory().unwrap(),
    );
    drop(kernel);
    let mut reopened = Kernel::from_connection(db, Box::new(DemoProvider), None).unwrap();
    let recovered = reopened.get_task(&agent.id).unwrap();
    assert_eq!(recovered.status, TaskStatus::Interrupted);
    let run = recovered.agent_run.unwrap();
    assert!(run.finished && run.outcome_unknown && run.pending_approvals.is_empty());
    assert!(
        reopened
            .respond_agent_approval(&agent.id, &approval.id, ApprovalDecision::AllowOnce)
            .is_err()
    );
    assert!(reopened.prepare_resume_task(&agent.id, true).is_err());
    assert_eq!(
        reopened.get_task(&tool.id).unwrap().status,
        TaskStatus::WaitingApproval
    );
    assert!(
        reopened
            .get_task(&tool.id)
            .unwrap()
            .pending_approval
            .is_some()
    );
}

#[test]
fn budget_and_run_identity_checks_prevent_unbounded_recording() {
    let (mut kernel, session) = setup();
    let (task, run) = start(&mut kernel, &session);
    assert!(
        kernel
            .observe_agent(
                &task.id,
                "old-run",
                AgentObservation::SessionOpened("wrong".into())
            )
            .is_err()
    );
    assert!(
        kernel
            .prepare_agent_run(&task.id, &CancellationToken::new())
            .is_err()
    );
    let before = kernel.list_events(&task.id, 0, 100).unwrap().events.len();
    assert!(
        kernel
            .observe_agent(
                &task.id,
                &run.request.run_id,
                AgentObservation::Update {
                    update: json!("x".repeat(256 * 1024)),
                    text: None
                }
            )
            .is_err()
    );
    assert_eq!(
        kernel.list_events(&task.id, 0, 100).unwrap().events.len(),
        before
    );
    for index in 0..16 {
        permission(&mut kernel, &task, &run, &format!("p{index}"), true);
    }
    let expiry = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
        + 1_000;
    assert!(
        kernel
            .observe_agent(
                &task.id,
                &run.request.run_id,
                AgentObservation::PermissionRequested {
                    permission_id: "overflow".into(),
                    request: json!({}),
                    allow_once_available: true,
                    expires_at_unix_ms: expiry,
                }
            )
            .is_err()
    );
    assert_eq!(
        kernel
            .get_task(&task.id)
            .unwrap()
            .agent_run
            .unwrap()
            .pending_approvals
            .len(),
        16
    );
}

#[test]
fn cancellation_wins_over_late_completion_and_closes_all_permissions() {
    let (mut kernel, session) = setup();
    let (task, run) = start(&mut kernel, &session);
    let approval = permission(&mut kernel, &task, &run, "one", true);
    kernel.cancel_task(&task.id).unwrap();
    assert!(
        kernel
            .respond_agent_approval(&task.id, &approval.id, ApprovalDecision::AllowOnce)
            .is_err()
    );
    assert!(
        kernel
            .observe_agent(
                &task.id,
                &run.request.run_id,
                AgentObservation::Update {
                    update: json!({}),
                    text: Some("late".into())
                }
            )
            .is_err()
    );
    let result = kernel
        .finish_agent_run(
            &task.id,
            &run.request.run_id,
            AgentOutcome {
                stop_reason: Some(AgentStopReason::EndTurn),
                outcome_unknown: false,
            },
        )
        .unwrap();
    assert_eq!(result.status, TaskStatus::Cancelled);
    assert!(result.output.is_none());
    assert!(result.agent_run.unwrap().pending_approvals.is_empty());
    let count = kernel.list_events(&task.id, 0, 100).unwrap().events.len();
    kernel
        .finish_agent_run(&task.id, &run.request.run_id, AgentOutcome::unknown())
        .unwrap();
    assert_eq!(
        kernel.list_events(&task.id, 0, 100).unwrap().events.len(),
        count
    );
}

#[test]
fn stop_reason_preserves_known_incomplete_outcomes_and_text() {
    let (mut kernel, session) = setup();
    for (reason, status) in [
        (AgentStopReason::EndTurn, TaskStatus::Completed),
        (AgentStopReason::Refusal, TaskStatus::Completed),
        (AgentStopReason::MaxTokens, TaskStatus::Interrupted),
        (AgentStopReason::MaxTurnRequests, TaskStatus::Interrupted),
    ] {
        let (task, run) = start(&mut kernel, &session);
        kernel
            .observe_agent(
                &task.id,
                &run.request.run_id,
                AgentObservation::Update {
                    update: json!({"text":"你好"}),
                    text: Some("你好".into()),
                },
            )
            .unwrap();
        let done = kernel
            .finish_agent_run(
                &task.id,
                &run.request.run_id,
                AgentOutcome {
                    stop_reason: Some(reason),
                    outcome_unknown: false,
                },
            )
            .unwrap();
        assert_eq!(done.status, status);
        assert_eq!(done.output.as_deref(), Some("你好"));
        assert!(!done.agent_run.unwrap().outcome_unknown);
    }
}
