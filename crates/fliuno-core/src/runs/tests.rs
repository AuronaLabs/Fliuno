use super::*;
use crate::{
    CancellationToken, DemoProvider, ModelDecision, ModelProvider, ModelTurn, PreparedModelStep,
    ToolSpec,
};
use fliuno_protocol::{Message, Session, ToolCall};

struct CompletingProvider;

struct NamedProvider(&'static str);

impl ModelProvider for NamedProvider {
    fn persistent_identity(&self) -> Option<(String, String)> {
        Some(("test".into(), self.0.into()))
    }

    fn next(
        &self,
        _task: &Task,
        _tools: &[ToolSpec],
        _cancellation: &CancellationToken,
    ) -> std::result::Result<ModelTurn, String> {
        Ok(ModelDecision::Complete("ok".into()).into())
    }
}

impl ModelProvider for CompletingProvider {
    fn persistent_identity(&self) -> Option<(String, String)> {
        Some(("test".into(), "completing-v1".into()))
    }

    fn next(
        &self,
        _task: &Task,
        _tools: &[ToolSpec],
        _cancellation: &CancellationToken,
    ) -> std::result::Result<ModelTurn, String> {
        Ok(ModelDecision::Complete("ok".into()).into())
    }
}

fn setup() -> (Kernel, Session) {
    let mut kernel = Kernel::in_memory(Box::new(DemoProvider)).unwrap();
    let workspace = kernel
        .open_workspace(std::env::current_dir().unwrap().to_str().unwrap())
        .unwrap();
    let session = kernel.create_session(&workspace.id).unwrap();
    kernel.set_model_provider(Box::new(CompletingProvider));
    (kernel, session)
}

fn request(session_id: &str, key: &str, nodes: Vec<RunNodeInput>) -> RunStartParams {
    RunStartParams {
        session_id: session_id.into(),
        client_request_id: key.into(),
        max_concurrency: 2,
        max_model_steps: None,
        timeout_ms: None,
        context: None,
        nodes,
    }
}

fn node(node_id: &str, input: &str, depends_on: &[&str]) -> RunNodeInput {
    RunNodeInput {
        node_id: node_id.into(),
        input: input.into(),
        depends_on: depends_on.iter().map(|id| (*id).into()).collect(),
    }
}

fn complete(kernel: &mut Kernel, task_id: &str, output: &str) {
    let PreparedModelStep::Ready { task, .. } = kernel.prepare_model_step(task_id).unwrap() else {
        panic!("child task was not ready")
    };
    kernel
        .apply_model_response(task, Ok(ModelDecision::Complete(output.into()).into()))
        .unwrap();
}

#[test]
fn run_dag_obeys_dependencies_concurrency_and_result_scope() {
    let (mut kernel, session) = setup();
    let (run, tasks, created) = kernel
        .start_run(request(
            &session.id,
            "dag-key",
            vec![
                node("prepare", "prepare", &[]),
                node("independent", "independent", &[]),
                node("finish", "finish", &["prepare"]),
            ],
        ))
        .unwrap();
    assert!(created);
    assert_eq!(tasks.len(), 2);
    let first = run.nodes[0].task_id.as_deref().unwrap();
    let independent = run.nodes[1].task_id.as_deref().unwrap();
    assert!(run.nodes[2].task_id.is_none());

    complete(&mut kernel, first, "prepared evidence");
    let (advanced, tasks) = kernel.advance_run(&run.id).unwrap();
    assert_eq!(tasks.len(), 1);
    let finish = advanced.nodes[2].task_id.as_deref().unwrap();
    let task = kernel.get_task(finish).unwrap();
    let Some(Message::User { text }) = task.messages.first() else {
        panic!("dependency child did not receive text")
    };
    assert!(text.contains("prepared evidence"));
    assert!(!text.contains("independent"));

    complete(&mut kernel, independent, "separate evidence");
    complete(&mut kernel, finish, "finished");
    let (finished, tasks) = kernel.advance_run(&run.id).unwrap();
    assert!(tasks.is_empty());
    assert_eq!(finished.status, RunStatus::Completed);
    assert!(
        finished
            .nodes
            .iter()
            .all(|node| node.status == RunNodeStatus::Completed)
    );
    let events = kernel
        .list_run_events(&RunEventsParams {
            run_id: run.id,
            after: 0,
            limit: 64,
        })
        .unwrap();
    assert_eq!(events.events.first().unwrap().data, RunEventKind::Created);
    assert!(matches!(
        events.events.last().unwrap().data,
        RunEventKind::Completed {
            status: RunStatus::Completed
        }
    ));
}

#[test]
fn waiting_for_approval_does_not_consume_a_run_execution_slot() {
    let mut kernel = Kernel::in_memory(Box::new(DemoProvider)).unwrap();
    let workspace = kernel
        .open_workspace(std::env::current_dir().unwrap().to_str().unwrap())
        .unwrap();
    let session = kernel.create_session(&workspace.id).unwrap();
    let mut request = request(
        &session.id,
        "approval-slot",
        vec![node("approval", "approval", &[]), node("next", "next", &[])],
    );
    request.max_concurrency = 1;
    let (run, tasks, _) = kernel.start_run(request).unwrap();
    assert_eq!(tasks.len(), 1);
    let PreparedModelStep::Ready { task, .. } = kernel.prepare_model_step(&tasks[0].id).unwrap()
    else {
        panic!("first run node was not ready")
    };
    let waiting = kernel
        .apply_model_response(
            task,
            Ok(ModelDecision::Call(ToolCall {
                id: "call-run-approval".into(),
                name: "artifact_create".into(),
                arguments: serde_json::json!({"title":"draft","content":"draft"}),
            })
            .into()),
        )
        .unwrap();
    assert_eq!(waiting.status, TaskStatus::WaitingApproval);
    let (advanced, tasks) = kernel.advance_run(&run.id).unwrap();
    assert_eq!(tasks.len(), 1);
    assert_eq!(advanced.nodes[0].status, RunNodeStatus::WaitingApproval);
    assert_eq!(advanced.nodes[1].status, RunNodeStatus::Running);
}

#[test]
fn run_start_is_idempotent_and_recovery_never_replays_a_child() {
    let (mut kernel, session) = setup();
    let request = request(&session.id, "retry-key", vec![node("only", "work", &[])]);
    let (run, tasks, created) = kernel.start_run(request.clone()).unwrap();
    assert!(created);
    assert_eq!(tasks.len(), 1);
    let (same, repeated, created) = kernel.start_run(request.clone()).unwrap();
    assert!(!created);
    assert!(repeated.is_empty());
    assert_eq!(same.id, run.id);
    let mut changed = request;
    changed.nodes[0].input = "different".into();
    assert!(kernel.start_run(changed).is_err());

    let task_id = run.nodes[0].task_id.clone().unwrap();
    assert!(matches!(
        kernel.prepare_model_step(&task_id).unwrap(),
        PreparedModelStep::Ready { .. }
    ));
    let database = std::mem::replace(
        &mut kernel.db,
        rusqlite::Connection::open_in_memory().unwrap(),
    );
    drop(kernel);
    let mut restored =
        Kernel::from_connection(database, Box::new(CompletingProvider), None).unwrap();
    let recovered_tasks = restored.advance_recoverable_runs().unwrap();
    assert!(recovered_tasks.is_empty());
    assert_eq!(
        restored.get_task(&task_id).unwrap().status,
        TaskStatus::Interrupted
    );
    assert_eq!(
        restored.get_run(&run.id).unwrap().status,
        RunStatus::Interrupted
    );
    assert!(
        restored
            .list_run_events(&RunEventsParams {
                run_id: run.id,
                after: 0,
                limit: 64,
            })
            .unwrap()
            .events
            .iter()
            .any(|event| matches!(
                event.data,
                RunEventKind::NodeStatusChanged {
                    status: RunNodeStatus::Interrupted,
                    ..
                }
            ))
    );
}

#[test]
fn startup_recovers_queued_children_only_with_original_authority() {
    for changed in [false, true] {
        let (mut kernel, session) = setup();
        let (run, tasks, _) = kernel
            .start_run(request(
                &session.id,
                "queued",
                vec![node("first", "queued", &[])],
            ))
            .unwrap();
        let task_id = &tasks[0].id;
        let database = std::mem::replace(
            &mut kernel.db,
            rusqlite::Connection::open_in_memory().unwrap(),
        );
        drop(kernel);
        let provider: Box<dyn ModelProvider> = if changed {
            Box::new(NamedProvider("different"))
        } else {
            Box::new(CompletingProvider)
        };
        let mut restored = Kernel::from_connection(database, provider, None).unwrap();
        let tasks = restored.advance_recoverable_runs().unwrap();
        if changed {
            assert!(tasks.is_empty());
            assert_eq!(
                restored.get_task(task_id).unwrap().status,
                TaskStatus::Failed
            );
            assert_eq!(restored.get_run(&run.id).unwrap().status, RunStatus::Failed);
        } else {
            assert_eq!(tasks.len(), 1);
            assert_eq!(&tasks[0].id, task_id);
            assert_eq!(tasks[0].model_steps, 0);
            complete(&mut restored, task_id, "resumed");
            assert_eq!(
                restored.advance_run(&run.id).unwrap().0.status,
                RunStatus::Completed
            );
        }
    }
}

#[test]
fn startup_recovers_after_a_committed_tool_result_and_after_child_completion() {
    let (mut kernel, session) = setup();
    let (run, tasks, _) = kernel
        .start_run(request(
            &session.id,
            "checkpoint",
            vec![
                node("first", "create a report", &[]),
                node("next", "summarize it", &["first"]),
            ],
        ))
        .unwrap();
    let task_id = &tasks[0].id;
    let PreparedModelStep::Ready { task, .. } = kernel.prepare_model_step(task_id).unwrap() else {
        panic!("no initial model step")
    };
    let pending = kernel
        .apply_model_response(
            task,
            Ok(ModelDecision::Call(ToolCall {
                id: "report-call".into(),
                name: "artifact_create".into(),
                arguments: serde_json::json!({"title":"report","content":"committed result"}),
            })
            .into()),
        )
        .unwrap();
    let approval = pending.pending_approval.unwrap();
    let crate::ApprovalOutcome::Prepared { execution, .. } = kernel
        .prepare_approval_response(
            task_id,
            &approval.id,
            fliuno_protocol::ApprovalDecision::AllowOnce,
        )
        .unwrap()
    else {
        panic!("tool approval did not prepare execution")
    };
    assert!(kernel.begin_tool_execution(&execution).unwrap());
    let result = execution.execute(&CancellationToken::new());
    assert_eq!(
        kernel
            .apply_tool_response(&execution, result)
            .unwrap()
            .status,
        TaskStatus::Running
    );
    let artifacts = kernel.list_artifacts(task_id).unwrap();
    assert_eq!(artifacts.len(), 1);

    let reopen = |kernel: Kernel| {
        Kernel::from_connection(kernel.db, Box::new(CompletingProvider), None).unwrap()
    };
    let mut restored = reopen(kernel);
    let recovered = restored.advance_recoverable_runs().unwrap();
    assert_eq!(recovered.len(), 1);
    assert_eq!(&recovered[0].id, task_id);
    assert_eq!(recovered[0].model_steps, 1);
    assert!(
        matches!(recovered[0].messages.last(), Some(Message::ToolResult { call_id, .. }) if call_id == "report-call")
    );
    complete(&mut restored, task_id, "finished report");

    let mut restored = reopen(restored);
    let descendants = restored.advance_recoverable_runs().unwrap();
    assert_eq!(descendants.len(), 1);
    assert_ne!(&descendants[0].id, task_id);
    assert!(
        matches!(descendants[0].messages.first(), Some(Message::User { text }) if text.contains("finished report"))
    );
    let retained = restored.list_artifacts(task_id).unwrap();
    assert_eq!(retained.len(), 1);
    assert_eq!(retained[0].id, artifacts[0].id);
    complete(&mut restored, &descendants[0].id, "summary");
    assert_eq!(
        restored.advance_run(&run.id).unwrap().0.status,
        RunStatus::Completed
    );
}

#[test]
fn invalid_run_graphs_are_rejected_before_persistence() {
    let (mut kernel, session) = setup();
    let cycle = request(
        &session.id,
        "cycle-key",
        vec![node("a", "a", &["b"]), node("b", "b", &["a"])],
    );
    assert!(kernel.start_run(cycle).is_err());
    let missing = request(
        &session.id,
        "missing-key",
        vec![node("a", "a", &["missing"])],
    );
    assert!(kernel.start_run(missing).is_err());
    let count: i64 = kernel
        .db
        .query_row("SELECT COUNT(*) FROM runs", [], |row| row.get(0))
        .unwrap();
    assert_eq!(count, 0);
}

#[test]
fn changed_model_identity_blocks_new_children_in_a_persisted_run() {
    let (mut kernel, session) = setup();
    kernel.set_model_provider(Box::new(NamedProvider("model-a")));
    let (run, tasks, _) = kernel
        .start_run(request(
            &session.id,
            "identity-key",
            vec![
                node("first", "first", &[]),
                node("next", "next", &["first"]),
            ],
        ))
        .unwrap();
    assert_eq!(tasks.len(), 1);
    complete(&mut kernel, &tasks[0].id, "done");
    kernel.set_model_provider(Box::new(NamedProvider("model-b")));
    let (run, tasks) = kernel.advance_run(&run.id).unwrap();
    assert!(tasks.is_empty());
    assert_eq!(run.status, RunStatus::PartiallyCompleted);
    assert_eq!(run.nodes[1].status, RunNodeStatus::Failed);
    assert_eq!(
        run.nodes[1].error.as_deref(),
        Some("run authority changed since creation")
    );
}

#[test]
fn a_failed_branch_skips_its_dependents_and_preserves_independent_output() {
    let (mut kernel, session) = setup();
    let (run, tasks, _) = kernel
        .start_run(request(
            &session.id,
            "partial",
            vec![
                node("broken", "will fail", &[]),
                node("independent", "finish", &[]),
                node("blocked", "blocked", &["broken"]),
            ],
        ))
        .unwrap();
    kernel
        .fail_task(&tasks[0].id, "test failure".into())
        .unwrap();
    complete(&mut kernel, &tasks[1].id, "usable output");
    let (settled, started) = kernel.advance_run(&run.id).unwrap();
    assert!(started.is_empty());
    assert_eq!(settled.status, RunStatus::PartiallyCompleted);
    assert_eq!(settled.nodes[2].status, RunNodeStatus::Skipped);
    assert!(settled.nodes[2].task_id.is_none());
    assert_eq!(
        kernel.get_task(&tasks[1].id).unwrap().output.as_deref(),
        Some("usable output")
    );
    let first = kernel
        .list_run_events(&RunEventsParams {
            run_id: run.id.clone(),
            after: 0,
            limit: 1,
        })
        .unwrap();
    let rest = kernel
        .list_run_events(&RunEventsParams {
            run_id: run.id,
            after: first.next_after,
            limit: 64,
        })
        .unwrap();
    assert!(
        rest.events
            .iter()
            .all(|event| event.sequence > first.next_after)
    );
    assert_eq!(rest.next_after, first.through_sequence);
    assert_eq!(
        rest.events.last().unwrap().data,
        RunEventKind::Completed {
            status: RunStatus::PartiallyCompleted
        }
    );
}

#[test]
fn cancelling_a_run_cancels_each_owned_task_and_is_audited() {
    let (mut kernel, session) = setup();
    let (run, tasks, _) = kernel
        .start_run(request(
            &session.id,
            "cancel-key",
            vec![node("one", "one", &[]), node("two", "two", &[])],
        ))
        .unwrap();
    kernel
        .db
        .execute_batch(
            "CREATE TRIGGER fail_run_cancel_event BEFORE INSERT ON run_events
         WHEN json_extract(NEW.data,'$.type')='cancelled'
         BEGIN SELECT RAISE(ABORT, 'injected cancellation failure'); END;",
        )
        .unwrap();
    assert!(matches!(
        kernel.cancel_run(&run.id),
        Err(Error::Database(_))
    ));
    assert_eq!(kernel.get_run(&run.id).unwrap().revision, run.revision);
    for task in &tasks {
        assert_eq!(
            kernel.get_task(&task.id).unwrap().status,
            TaskStatus::Running
        );
    }
    kernel
        .db
        .execute_batch("DROP TRIGGER fail_run_cancel_event;")
        .unwrap();
    let (cancelled, ids) = kernel.cancel_run(&run.id).unwrap();
    assert_eq!(ids.len(), 2);
    assert_eq!(cancelled.status, RunStatus::Cancelled);
    assert!(
        cancelled
            .nodes
            .iter()
            .all(|node| node.status == RunNodeStatus::Cancelled)
    );
    assert!(
        tasks
            .iter()
            .all(|task| kernel.get_task(&task.id).unwrap().status == TaskStatus::Cancelled)
    );
}

#[test]
fn a_failed_start_event_rolls_back_the_child_and_its_ownership() {
    let (mut kernel, session) = setup();
    kernel
        .db
        .execute_batch(
            "CREATE TRIGGER fail_run_start_event BEFORE INSERT ON run_events
         WHEN json_extract(NEW.data,'$.type')='node_started'
         BEGIN SELECT RAISE(ABORT, 'injected start event failure'); END;",
        )
        .unwrap();
    let (run, tasks, _) = kernel
        .start_run(request(
            &session.id,
            "rollback",
            vec![node("child", "work", &[])],
        ))
        .unwrap();
    assert!(tasks.is_empty());
    assert_eq!(run.status, RunStatus::Failed);
    assert!(run.nodes[0].task_id.is_none());
    let counts: (i64, i64, i64) = kernel
        .db
        .query_row(
            "SELECT (SELECT COUNT(*) FROM tasks), (SELECT COUNT(*) FROM events),
                (SELECT COUNT(task_id) FROM run_nodes)",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(counts, (0, 0, 0));
}

#[test]
fn recovery_reconciles_an_owned_child_when_the_run_snapshot_lags() {
    for restart in [false, true] {
        let (mut kernel, session) = setup();
        let request = request(&session.id, "lagging", vec![node("child", "work", &[])]);
        kernel
            .db
            .execute_batch(
                "CREATE TRIGGER fail_run_projection BEFORE UPDATE ON runs
             BEGIN SELECT RAISE(ABORT, 'injected projection failure'); END;",
            )
            .unwrap();
        assert!(matches!(
            kernel.start_run(request.clone()),
            Err(Error::Database(_))
        ));
        let child_id: String = kernel
            .db
            .query_row("SELECT task_id FROM run_nodes", [], |row| row.get(0))
            .unwrap();
        kernel
            .db
            .execute_batch("DROP TRIGGER fail_run_projection;")
            .unwrap();
        if restart {
            assert!(matches!(
                kernel.prepare_model_step(&child_id).unwrap(),
                PreparedModelStep::Ready { .. }
            ));
            let database = std::mem::replace(
                &mut kernel.db,
                rusqlite::Connection::open_in_memory().unwrap(),
            );
            drop(kernel);
            kernel = Kernel::from_connection(database, Box::new(CompletingProvider), None).unwrap();
            assert!(kernel.advance_recoverable_runs().unwrap().is_empty());
        }
        let (run, tasks, created) = kernel.start_run(request).unwrap();
        assert!(!created);
        assert_eq!(tasks.len(), usize::from(!restart));
        assert_eq!(run.nodes[0].task_id.as_deref(), Some(child_id.as_str()));
        if restart {
            assert_eq!(run.status, RunStatus::Interrupted);
            assert!(kernel.get_task(&child_id).unwrap().model_outcome_unknown);
        } else {
            assert_eq!(tasks[0].id, child_id);
        }
        let events = kernel
            .list_run_events(&RunEventsParams {
                run_id: run.id,
                after: 0,
                limit: 64,
            })
            .unwrap();
        assert_eq!(
            events
                .events
                .iter()
                .filter(|event| matches!(event.data, RunEventKind::NodeStarted { .. }))
                .count(),
            1
        );
    }
}

#[test]
fn startup_expires_an_overdue_run_without_replaying_dispatched_work() {
    let (mut kernel, session) = setup();
    let mut request = request(
        &session.id,
        "offline-timeout",
        vec![
            node("done", "keep", &[]),
            node("active", "call", &[]),
            node("later", "never start", &["active"]),
        ],
    );
    request.timeout_ms = Some(MAX_RUN_TIMEOUT_MS);
    let (run, tasks, _) = kernel.start_run(request.clone()).unwrap();
    let mut different = request;
    different.timeout_ms = Some(MAX_RUN_TIMEOUT_MS - 1);
    assert!(matches!(
        kernel.start_run(different),
        Err(Error::Conflict(_))
    ));
    complete(&mut kernel, &tasks[0].id, "saved output");
    assert!(matches!(
        kernel.prepare_model_step(&tasks[1].id).unwrap(),
        PreparedModelStep::Ready { .. }
    ));

    // Move the fixture's deadline into the past without a wall-clock sleep.
    kernel
        .db
        .execute(
            "UPDATE runs SET data=json_set(data,'$.run.deadlineAtUnixMs',1) WHERE id=?1",
            [&run.id],
        )
        .unwrap();
    kernel
        .db
        .execute(
            "UPDATE run_deadlines SET deadline_at_unix_ms=1 WHERE run_id=?1",
            [&run.id],
        )
        .unwrap();
    let database = std::mem::replace(
        &mut kernel.db,
        rusqlite::Connection::open_in_memory().unwrap(),
    );
    drop(kernel);
    let mut restored =
        Kernel::from_connection(database, Box::new(CompletingProvider), None).unwrap();
    assert!(restored.advance_recoverable_runs().unwrap().is_empty());
    let expired = restored.get_run(&run.id).unwrap();
    assert_eq!(expired.status, RunStatus::TimedOut);
    assert_eq!(expired.nodes[0].status, RunNodeStatus::Completed);
    assert_eq!(expired.nodes[1].status, RunNodeStatus::TimedOut);
    assert_eq!(expired.nodes[2].status, RunNodeStatus::TimedOut);
    assert!(expired.nodes[2].task_id.is_none());
    assert_eq!(
        restored.get_task(&tasks[0].id).unwrap().output.as_deref(),
        Some("saved output")
    );
    let cancelled = restored.get_task(&tasks[1].id).unwrap();
    assert_eq!(cancelled.status, TaskStatus::Cancelled);
    assert!(cancelled.model_outcome_unknown);
    assert!(restored.expire_next_run().unwrap().is_none());
}
