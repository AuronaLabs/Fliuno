use super::*;
use serde_json::json;
#[path = "tests/selection.rs"]
mod selection;

fn setup() -> (Kernel, Session) {
    let mut kernel = Kernel::in_memory(Box::new(DemoProvider)).unwrap();
    let workspace = kernel
        .open_workspace(std::env::current_dir().unwrap().to_str().unwrap())
        .unwrap();
    let session = kernel.create_session(&workspace.id).unwrap();
    (kernel, session)
}

fn complete(kernel: &mut Kernel, session: &str, input: &str, output: &str) -> Task {
    let task = kernel.create_task_with_key(session, input, None).unwrap().0;
    finish(kernel, &task.id, output)
}

fn finish(kernel: &mut Kernel, id: &str, output: &str) -> Task {
    let PreparedModelStep::Ready { task, .. } = kernel.prepare_model_step(id).unwrap() else {
        panic!("not ready")
    };
    kernel
        .apply_model_response(
            task,
            Ok(ModelTurn {
                decision: ModelDecision::Complete(output.into()),
                replay: Some(ProviderReplay {
                    provider: "test".into(),
                    data: json!({"signature":"private-reasoning"}),
                }),
            }),
        )
        .unwrap()
}

#[test]
fn history_contains_real_trace_and_artifact_identity_without_inheriting_execution() {
    let (mut kernel, session) = setup();
    let first = kernel.start_task(&session.id, "创建中文报告").unwrap();
    let approval = first.pending_approval.clone().unwrap();
    let done = kernel
        .respond_approval(&first.id, &approval.id, ApprovalDecision::AllowOnce)
        .unwrap();
    let next = kernel.start_task(&session.id, "修改报告标题").unwrap();
    let context = next.context.as_ref().unwrap();
    assert_eq!(context.turns.len(), 1);
    assert_eq!(context.turns[0].task_id, done.id);
    assert!(
        context.turns[0]
            .messages
            .iter()
            .any(|m| matches!(m, ContextMessage::ToolResult { .. }))
    );
    assert_eq!(
        context.turns[0].artifacts[0].id,
        kernel.list_artifacts(&done.id).unwrap()[0].id
    );
    assert_eq!(next.messages.len(), 2);
    assert_eq!(next.status, TaskStatus::WaitingApproval);
    assert_ne!(next.pending_approval.as_ref().unwrap().id, approval.id);
    assert!(
        kernel
            .respond_approval(&next.id, &approval.id, ApprovalDecision::AllowOnce)
            .is_err()
    );
    assert!(kernel.list_artifacts(&next.id).unwrap().is_empty());
}

#[test]
fn snapshot_and_idempotency_stay_frozen_when_other_tasks_complete() {
    let (mut kernel, session) = setup();
    complete(&mut kernel, &session.id, "remember", "answer");
    let (task, _) = kernel
        .create_task_with_key(&session.id, "follow up", Some("stable"))
        .unwrap();
    let original = serde_json::to_value(task.context.as_ref().unwrap()).unwrap();
    complete(&mut kernel, &session.id, "later", "later answer");
    let (retried, created) = kernel
        .create_task_with_key(&session.id, "follow up", Some("stable"))
        .unwrap();
    assert!(!created);
    assert_eq!(serde_json::to_value(retried.context).unwrap(), original);
    assert!(
        !context_prompt(task.context.as_ref())
            .unwrap()
            .unwrap()
            .contains("private-reasoning")
    );
    let changed = ContextOptions {
        mode: ContextMode::Isolated,
        ..ContextOptions::default()
    };
    assert!(
        kernel
            .create_task_with_context(
                &session.id,
                "follow up",
                Some("stable"),
                TaskBackend::Model,
                Some(changed)
            )
            .is_err()
    );
}

#[test]
fn history_follows_completion_order_and_never_crosses_session_scope() {
    let (mut kernel, session) = setup();
    let first = kernel
        .create_task_with_key(&session.id, "started first", None)
        .unwrap()
        .0;
    let second = kernel
        .create_task_with_key(&session.id, "started second", None)
        .unwrap()
        .0;
    finish(&mut kernel, &second.id, "completed first");
    let snapshot = kernel
        .create_task_with_key(&session.id, "freeze", None)
        .unwrap()
        .0;
    finish(&mut kernel, &first.id, "completed second");
    assert_eq!(snapshot.context.unwrap().turns[0].task_id, second.id);
    let context = kernel
        .session_context(&session.id, ContextOptions::default())
        .unwrap();
    assert_eq!(
        context
            .turns
            .iter()
            .map(|t| t.task_id.as_str())
            .collect::<Vec<_>>(),
        vec![second.id.as_str(), first.id.as_str()]
    );
    let other = kernel.create_session(&session.workspace_id).unwrap();
    assert!(
        kernel
            .session_context(&other.id, ContextOptions::default())
            .unwrap()
            .turns
            .is_empty()
    );
    assert!(
        kernel
            .session_context("missing", ContextOptions::default())
            .is_err()
    );
}

#[test]
fn selection_bounds_whole_turns_and_reports_every_omission() {
    let (mut kernel, session) = setup();
    for index in 0..35 {
        complete(&mut kernel, &session.id, &format!("input {index}"), "你好");
    }
    let context = kernel
        .session_context(
            &session.id,
            ContextOptions {
                max_turns: 32,
                ..ContextOptions::default()
            },
        )
        .unwrap();
    assert_eq!(context.turns.len(), 32);
    assert_eq!(context.omitted_turns, 3);
    let bounded = kernel
        .session_context(
            &session.id,
            ContextOptions {
                max_bytes: 1024,
                ..ContextOptions::default()
            },
        )
        .unwrap();
    assert!(serde_json::to_vec(&bounded).unwrap().len() <= 1024);
    assert_eq!(bounded.turns.len() as u64 + bounded.omitted_turns, 35);
    complete(
        &mut kernel,
        &session.id,
        "largest newest",
        &"大".repeat(90_000),
    );
    let context = kernel
        .session_context(&session.id, ContextOptions::default())
        .unwrap();
    assert!(context.turns.is_empty());
    assert_eq!(context.omitted_turns, 36);
    assert!(
        context_prompt(Some(&context))
            .unwrap()
            .unwrap()
            .contains("\"omittedTurns\":36")
    );
    let isolated = kernel
        .session_context(
            &session.id,
            ContextOptions {
                mode: ContextMode::Isolated,
                ..ContextOptions::default()
            },
        )
        .unwrap();
    assert!(isolated.turns.is_empty());
    assert!(context_prompt(Some(&isolated)).unwrap().is_none());
    for (max_turns, max_bytes) in [(0, 1024), (33, 1024), (1, 1023), (1, 262145)] {
        assert!(
            kernel
                .session_context(
                    &session.id,
                    ContextOptions {
                        max_turns,
                        max_bytes,
                        ..ContextOptions::default()
                    }
                )
                .is_err()
        );
    }
}

#[test]
fn completion_projection_failure_rolls_back_task_and_event_together() {
    let (mut kernel, session) = setup();
    let task = kernel
        .create_task_with_key(&session.id, "atomic", None)
        .unwrap()
        .0;
    let PreparedModelStep::Ready { task, .. } = kernel.prepare_model_step(&task.id).unwrap() else {
        panic!("not ready")
    };
    let before = kernel.list_events(&task.id, 0, 100).unwrap().events.len();
    kernel.db.execute_batch("CREATE TRIGGER reject_history BEFORE INSERT ON session_turns BEGIN SELECT RAISE(ABORT,'fixture'); END;").unwrap();
    assert!(
        kernel
            .apply_model_response(
                task.clone(),
                Ok(ModelDecision::Complete("done".into()).into())
            )
            .is_err()
    );
    let current = kernel.get_task(&task.id).unwrap();
    assert_eq!(current.status, TaskStatus::Running);
    assert!(current.model_outcome_unknown);
    assert_eq!(
        kernel.list_events(&task.id, 0, 100).unwrap().events.len(),
        before
    );
    assert!(
        kernel
            .session_context(&session.id, ContextOptions::default())
            .unwrap()
            .turns
            .is_empty()
    );
}

#[test]
fn version_four_backfill_preserves_old_inputs_events_and_idempotency() {
    let (mut kernel, session) = setup();
    let (task, _) = kernel
        .create_task_with_key(&session.id, "legacy", Some("old-key"))
        .unwrap();
    finish(&mut kernel, &task.id, "old answer");
    let pending = kernel
        .create_task_with_key(&session.id, "old active", None)
        .unwrap()
        .0;
    kernel
        .db
        .execute("UPDATE tasks SET data=json_remove(data,'$.context')", [])
        .unwrap();
    let before = serde_json::to_value(kernel.list_all_events(0, 100).unwrap()).unwrap();
    kernel
        .db
        .execute_batch("DROP TABLE session_turns; PRAGMA user_version=4;")
        .unwrap();
    let db = std::mem::replace(&mut kernel.db, Connection::open_in_memory().unwrap());
    drop(kernel);
    let mut restored = Kernel::from_connection(db, Box::new(DemoProvider), None).unwrap();
    let legacy = restored.get_task(&task.id).unwrap();
    assert!(legacy.context.is_none());
    let (retry, created) = restored
        .create_task_with_key(&session.id, "legacy", Some("old-key"))
        .unwrap();
    assert!(!created && retry.context.is_none());
    let context = restored
        .session_context(&session.id, ContextOptions::default())
        .unwrap();
    assert_eq!(context.turns[0].task_id, task.id);
    assert!(restored.get_task(&pending.id).unwrap().context.is_none());
    let after = restored.list_all_events(0, 100).unwrap();
    assert_eq!(
        &serde_json::to_value(&after).unwrap()["events"]
            .as_array()
            .unwrap()[..before["events"].as_array().unwrap().len()],
        before["events"].as_array().unwrap().as_slice()
    );
    assert_eq!(
        restored
            .db
            .pragma_query_value(None, "user_version", |r| r.get::<_, u32>(0))
            .unwrap(),
        9
    );
}

#[test]
fn incomplete_work_is_excluded_and_explicit_resume_keeps_original_input_context() {
    let (mut kernel, session) = setup();
    let first = complete(&mut kernel, &session.id, "first", "known answer");
    kernel
        .start_task(&session.id, "waiting for approval")
        .unwrap();
    let cancelled = kernel
        .create_task_with_key(&session.id, "cancel", None)
        .unwrap()
        .0;
    kernel.cancel_task(&cancelled.id).unwrap();
    let failed = kernel
        .create_task_with_key(&session.id, "fail", None)
        .unwrap()
        .0;
    kernel
        .fail_task(&failed.id, "fixture failure".into())
        .unwrap();
    let uncertain = kernel
        .create_task_with_key(&session.id, "uncertain", None)
        .unwrap()
        .0;
    let frozen = serde_json::to_value(&uncertain.context).unwrap();
    kernel.prepare_model_step(&uncertain.id).unwrap();
    kernel.interrupt_running_tasks().unwrap();
    assert!(
        kernel
            .get_task(&uncertain.id)
            .unwrap()
            .model_outcome_unknown
    );
    let context = kernel
        .session_context(&session.id, ContextOptions::default())
        .unwrap();
    assert_eq!(context.turns.len(), 1);
    assert_eq!(context.turns[0].task_id, first.id);
    complete(&mut kernel, &session.id, "later", "new answer");
    assert!(kernel.prepare_resume_task(&uncertain.id, false).is_err());
    let resumed = kernel.prepare_resume_task(&uncertain.id, true).unwrap();
    assert_eq!(serde_json::to_value(resumed.context).unwrap(), frozen);
    assert_eq!(
        kernel
            .session_context(&session.id, ContextOptions::default())
            .unwrap()
            .turns
            .len(),
        2
    );
}

#[test]
fn projection_rejects_unfinished_agent_and_unknown_host_operations() {
    let (mut kernel, session) = setup();
    let mut task = complete(&mut kernel, &session.id, "known", "answer");
    let mut run = AgentRun {
        remote_task: None,
        artifacts: Vec::new(),
        id: "fixture".into(),
        finished: true,
        session_id: None,
        outcome_unknown: false,
        stop_reason: Some(AgentStopReason::EndTurn),
        text: "answer".into(),
        update_count: 1,
        recorded_bytes: 6,
        permission_count: 0,
        pending_approvals: vec![],
        host_tools: vec![],
    };
    for (finished, unknown) in [(false, false), (true, true)] {
        run.finished = finished;
        run.outcome_unknown = unknown;
        task.agent_run = Some(run.clone());
        assert!(project_turn(&task, 1, vec![]).is_none());
    }
    run.finished = true;
    run.outcome_unknown = false;
    for status in [
        AgentHostToolStatus::AwaitingApproval,
        AgentHostToolStatus::Approved,
        AgentHostToolStatus::Running,
        AgentHostToolStatus::OutcomeUnknown,
    ] {
        run.host_tools = vec![AgentHostTool {
            id: "operation".into(),
            call: ToolCall {
                id: "call".into(),
                name: "workspace_read_text".into(),
                arguments: json!({"path":"file.txt"}),
            },
            status,
            output: None,
            error: None,
        }];
        task.agent_run = Some(run.clone());
        assert!(project_turn(&task, 1, vec![]).is_none());
    }
    run.host_tools.clear();
    task.agent_run = Some(run);
    assert!(project_turn(&task, 1, vec![]).is_some());
}
