use super::*;
use fliuno_protocol::TaskStatus;

fn task() -> TaskSummary {
    TaskSummary {
        id: "task-a".into(),
        session_id: "session-a".into(),
        status: TaskStatus::Interrupted,
        backend: Default::default(),
        input_preview: String::new(),
        pending_approval_count: 0,
        model_outcome_unknown: false,
        agent_outcome_unknown: false,
    }
}

fn event(sequence: u64, data: EventKind) -> Event {
    Event {
        sequence,
        task_id: "task-a".into(),
        data,
    }
}

#[test]
fn resume_after_interruption_projects_a_separate_run_with_stable_wire_ids() {
    fn replay() -> Vec<Value> {
        let mut projector = AgUiProjector::new(&task());
        [
            event(7, EventKind::TaskStarted),
            event(9, EventKind::TaskInterrupted),
            event(15, EventKind::TaskResumed),
            event(
                28,
                EventKind::TaskCompleted {
                    text: "你好".into(),
                },
            ),
        ]
        .into_iter()
        .flat_map(|event| projector.project(event).unwrap())
        .map(|event| serde_json::to_value(event).unwrap())
        .collect()
    }
    let events = replay();
    assert_eq!(events, replay());
    assert_eq!(
        events,
        vec![
            json!({"type":"RUN_STARTED","threadId":"session-a","runId":"task-a:run:7"}),
            json!({"type":"RUN_ERROR","code":"fliuno.task_interrupted","message":"Task interrupted"}),
            json!({"type":"RUN_STARTED","threadId":"session-a","runId":"task-a:run:15"}),
            json!({"type":"TEXT_MESSAGE_START","messageId":"task-a:message:28","role":"assistant"}),
            json!({"type":"TEXT_MESSAGE_CONTENT","messageId":"task-a:message:28","delta":"你好"}),
            json!({"type":"TEXT_MESSAGE_END","messageId":"task-a:message:28"}),
            json!({"type":"RUN_FINISHED","threadId":"session-a","runId":"task-a:run:15"}),
        ]
    );
}

#[test]
fn projector_rejects_missing_history_and_mixed_tasks_without_advancing() {
    let mut projector = AgUiProjector::new(&task());
    assert_eq!(
        projector
            .project(event(99, EventKind::TaskResumed))
            .unwrap_err(),
        AgUiProjectionError::Lifecycle
    );
    projector.project(event(7, EventKind::TaskStarted)).unwrap();
    assert_eq!(
        projector
            .project(event(7, EventKind::TaskInterrupted))
            .unwrap_err(),
        AgUiProjectionError::Sequence
    );
    let mut wrong_task = event(200, EventKind::TaskInterrupted);
    wrong_task.task_id = "task-b".into();
    assert_eq!(
        projector.project(wrong_task).unwrap_err(),
        AgUiProjectionError::WrongTask
    );
    projector
        .project(event(10, EventKind::TaskInterrupted))
        .unwrap();
    let cancel = projector
        .project(event(11, EventKind::TaskCancelled))
        .unwrap();
    assert_eq!(
        serde_json::to_value(cancel).unwrap(),
        json!([
            {"type":"CUSTOM","name":"fliuno.task_cancelled","value":{"taskId":"task-a"}}
        ])
    );
}
