use super::*;

#[test]
fn remote_identity_and_terminal_evidence_survive_local_cancellation() {
    let (mut kernel, session) = setup();
    let (task, run) = start(&mut kernel, &session);
    let remote = AgentRemoteTask {
        id: "remote".into(),
        context_id: Some("context".into()),
        state: AgentTaskState::Working,
    };
    kernel
        .observe_agent(
            &task.id,
            &run.request.run_id,
            AgentObservation::RemoteTask(remote.clone()),
        )
        .unwrap();
    let count = kernel.list_events(&task.id, 0, 100).unwrap().events.len();
    kernel
        .observe_agent(
            &task.id,
            &run.request.run_id,
            AgentObservation::RemoteTask(remote.clone()),
        )
        .unwrap();
    assert_eq!(
        kernel.list_events(&task.id, 0, 100).unwrap().events.len(),
        count
    );
    for changed in [
        AgentRemoteTask {
            id: "other".into(),
            ..remote.clone()
        },
        AgentRemoteTask {
            context_id: None,
            ..remote.clone()
        },
    ] {
        assert!(
            kernel
                .observe_agent(
                    &task.id,
                    &run.request.run_id,
                    AgentObservation::RemoteTask(changed)
                )
                .is_err()
        );
    }
    kernel.cancel_task(&task.id).unwrap();
    kernel
        .observe_agent(
            &task.id,
            &run.request.run_id,
            AgentObservation::RemoteTask(AgentRemoteTask {
                state: AgentTaskState::Cancelled,
                ..remote.clone()
            }),
        )
        .unwrap();
    assert!(
        kernel
            .observe_agent(
                &task.id,
                &run.request.run_id,
                AgentObservation::RemoteTask(remote)
            )
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
    let task = kernel
        .finish_agent_run(
            &task.id,
            &run.request.run_id,
            AgentOutcome {
                stop_reason: Some(AgentStopReason::Cancelled),
                outcome_unknown: false,
            },
        )
        .unwrap();
    assert_eq!(task.status, TaskStatus::Cancelled);
    let run = task.agent_run.unwrap();
    assert!(!run.outcome_unknown);
    assert_eq!(run.remote_task.unwrap().state, AgentTaskState::Cancelled);
}

#[test]
fn artifact_import_is_atomic_bounded_and_keeps_local_and_remote_identities_separate() {
    let (mut kernel, session) = setup();
    let (task, run) = start(&mut kernel, &session);
    let make = |source: &str| IncomingAgentArtifact {
        source_id: source.into(),
        title: "report".into(),
        media_type: "application/json".into(),
        content: json!({"url":"file:///must-not-open","text":"review only"}).to_string(),
    };
    let count = kernel.list_events(&task.id, 0, 100).unwrap().events.len();
    assert!(
        kernel
            .observe_agent(
                &task.id,
                &run.request.run_id,
                AgentObservation::Artifacts(vec![make("duplicate"), make("duplicate")])
            )
            .is_err()
    );
    assert!(kernel.list_artifacts(&task.id).unwrap().is_empty());
    assert_eq!(
        kernel.list_events(&task.id, 0, 100).unwrap().events.len(),
        count
    );
    let mut large = make("large");
    large.content = "x".repeat(262_145);
    assert!(
        kernel
            .observe_agent(
                &task.id,
                &run.request.run_id,
                AgentObservation::Artifacts(vec![make("valid"), large])
            )
            .is_err()
    );
    assert!(kernel.list_artifacts(&task.id).unwrap().is_empty());
    kernel
        .observe_agent(
            &task.id,
            &run.request.run_id,
            AgentObservation::Artifacts(vec![make("peer-id")]),
        )
        .unwrap();
    assert!(
        kernel
            .observe_agent(
                &task.id,
                &run.request.run_id,
                AgentObservation::Artifacts(vec![make("new-id")])
            )
            .is_err()
    );
    let artifacts = kernel.list_artifacts(&task.id).unwrap();
    assert_eq!(artifacts.len(), 1);
    assert_ne!(artifacts[0].id, "peer-id");
    let links = kernel
        .get_task(&task.id)
        .unwrap()
        .agent_run
        .unwrap()
        .artifacts;
    assert_eq!(links[0].source_id, "peer-id");
    assert_eq!(links[0].artifact_id, artifacts[0].id);
}

#[test]
fn explicit_remote_failure_and_pause_reasons_never_complete_a_task() {
    for (reason, status) in [
        (AgentStopReason::Failed, TaskStatus::Failed),
        (AgentStopReason::Rejected, TaskStatus::Failed),
        (AgentStopReason::InputRequired, TaskStatus::Interrupted),
        (AgentStopReason::AuthRequired, TaskStatus::Interrupted),
    ] {
        let (mut kernel, session) = setup();
        let (task, run) = start(&mut kernel, &session);
        let task = kernel
            .finish_agent_run(
                &task.id,
                &run.request.run_id,
                AgentOutcome {
                    stop_reason: Some(reason),
                    outcome_unknown: false,
                },
            )
            .unwrap();
        assert_eq!(task.status, status);
        assert!(!task.agent_run.unwrap().outcome_unknown);
        assert!(
            kernel
                .session_context(&session.id, ContextOptions::default())
                .unwrap()
                .turns
                .is_empty()
        );
    }
}

#[test]
fn version_six_agent_snapshots_default_remote_evidence_without_replaying() {
    let (mut kernel, session) = setup();
    let (task, _run) = start(&mut kernel, &session);
    let mut snapshot = serde_json::to_value(kernel.get_task(&task.id).unwrap()).unwrap();
    snapshot["agentRun"]
        .as_object_mut()
        .unwrap()
        .remove("remoteTask");
    snapshot["agentRun"]
        .as_object_mut()
        .unwrap()
        .remove("artifacts");
    kernel
        .db
        .execute(
            "UPDATE tasks SET data=?1 WHERE id=?2",
            rusqlite::params![snapshot.to_string(), task.id],
        )
        .unwrap();
    kernel.db.pragma_update(None, "user_version", 6).unwrap();
    let db = std::mem::replace(
        &mut kernel.db,
        rusqlite::Connection::open_in_memory().unwrap(),
    );
    drop(kernel);
    let recovered = Kernel::from_connection(db, Box::new(DemoProvider), None).unwrap();
    let task = recovered.get_task(&task.id).unwrap();
    assert_eq!(task.status, TaskStatus::Interrupted);
    let run = task.agent_run.unwrap();
    assert!(
        run.remote_task.is_none()
            && run.artifacts.is_empty()
            && run.outcome_unknown
            && run.finished
    );
    assert_eq!(
        recovered
            .db
            .pragma_query_value(None, "user_version", |r| r.get::<_, u32>(0))
            .unwrap(),
        10
    );
}
