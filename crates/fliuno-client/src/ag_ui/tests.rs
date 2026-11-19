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

#[test]
fn sse_resumes_after_a_partial_write_inside_one_completion() {
    struct Cutoff {
        data: Vec<u8>,
        remaining: usize,
    }
    impl std::io::Write for Cutoff {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            let count = self.remaining.min(bytes.len());
            if count == 0 {
                return Err(std::io::ErrorKind::BrokenPipe.into());
            }
            self.data.extend_from_slice(&bytes[..count]);
            self.remaining -= count;
            Ok(count)
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    let started = event(7, EventKind::TaskStarted);
    let text = "你好\n\nid: foreign\r\n\0";
    let finished = event(11, EventKind::TaskCompleted { text: text.into() });
    let mut full = Vec::new();
    let mut stream = AgUiSseStream::new(&task(), None).unwrap();
    assert_eq!(stream.write_event(started.clone(), &mut full).unwrap(), 1);
    assert_eq!(stream.write_event(finished.clone(), &mut full).unwrap(), 4);
    let full = String::from_utf8(full).unwrap();
    let frames = full
        .trim_end_matches('\n')
        .split("\n\n")
        .collect::<Vec<_>>();
    assert_eq!(frames.len(), 5);
    let (last_id, data) = frames[2].split_once('\n').unwrap();
    let last_id = last_id.strip_prefix("id: ").unwrap();
    let content: Value = serde_json::from_str(data.strip_prefix("data: ").unwrap()).unwrap();
    assert_eq!(last_id, "task-a:11:2");
    assert_eq!(content["delta"], text);
    let handled = frames[..3]
        .iter()
        .map(|frame| frame.len() + 2)
        .sum::<usize>();
    let mut partial = Cutoff {
        data: Vec::new(),
        remaining: handled + 3,
    };
    let mut stream = AgUiSseStream::new(&task(), None).unwrap();
    stream.write_event(started.clone(), &mut partial).unwrap();
    assert!(matches!(
        stream.write_event(finished.clone(), &mut partial),
        Err(AgUiSseError::Output(_))
    ));
    assert_eq!(partial.data, full.as_bytes()[..handled + 3]);
    assert!(matches!(
        stream.write_event(finished.clone(), &mut partial),
        Err(AgUiSseError::Closed)
    ));

    let mut resumed = AgUiSseStream::new(&task(), Some(last_id)).unwrap();
    let mut rest = Vec::new();
    assert_eq!(resumed.write_event(started, &mut rest).unwrap(), 0);
    assert_eq!(resumed.write_event(finished, &mut rest).unwrap(), 2);
    resumed.finish_replay().unwrap();
    partial.data.truncate(handled);
    partial.data.extend(rest);
    assert_eq!(partial.data, full.as_bytes());
}

#[test]
fn sse_refuses_a_cursor_without_a_matching_emitted_frame() {
    for id in ["task-b:7:1", "task-a:07:1", "task-a:7:0", "task-a:7:1\n"] {
        assert!(matches!(
            AgUiSseStream::new(&task(), Some(id)),
            Err(AgUiSseError::InvalidResumeId)
        ));
    }
    let mut output = Vec::new();
    let mut invalid_slot = AgUiSseStream::new(&task(), Some("task-a:7:2")).unwrap();
    assert!(matches!(
        invalid_slot.write_event(event(7, EventKind::TaskStarted), &mut output),
        Err(AgUiSseError::UnknownResumeId)
    ));
    let mut beyond_end = AgUiSseStream::new(&task(), Some("task-a:17:1")).unwrap();
    beyond_end
        .write_event(event(7, EventKind::TaskStarted), &mut output)
        .unwrap();
    assert!(matches!(
        beyond_end.finish_replay(),
        Err(AgUiSseError::UnknownResumeId)
    ));
    assert!(output.is_empty());
}
