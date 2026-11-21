use super::*;
use fliuno_client::{EventFeed, WorkSubscription};
use fliuno_protocol::{Session, WorkSnapshotParams};
use std::process::ChildStdout;

type LocalClient = Client<BufReader<ChildStdout>, ChildStdin>;

fn wait_for_status(client: &mut LocalClient, id: &str, status: TaskStatus) -> Task {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let task: Task = client
            .call("task/get", &json!({"taskId":id}), |_| {})
            .unwrap();
        if task.status == status {
            return task;
        }
        assert!(!task.status.is_terminal() && Instant::now() < deadline);
        thread::sleep(Duration::from_millis(5));
    }
}

fn drain(feed: &mut EventFeed, client: &mut LocalClient) -> Vec<Event> {
    let mut events = Vec::new();
    while let Some(page) = feed.next_page(client, 2, None, |_| {}).unwrap() {
        events.extend(page);
    }
    events
}

#[test]
fn headless_workflow_recovers_lost_progress_and_delivers_one_approved_artifact() {
    let root = std::env::temp_dir().join(format!("fliuno-headless-{}", uuid::Uuid::new_v4()));
    fs::create_dir(&root).unwrap();
    let db = root.join("kernel.db");
    let (server, mut client) = super::ag_ui_run::connect(&db);
    let workspace: Workspace = client
        .call("workspace/open", &json!({"root":root}), |_| {})
        .unwrap();
    let session: Session = client
        .call(
            "session/create",
            &json!({"workspaceId":workspace.id}),
            |_| {},
        )
        .unwrap();
    let WorkSubscription {
        snapshot, events, ..
    } = client
        .subscribe_work(
            &WorkSnapshotParams {
                workspace_id: None,
                session_id: Some(session.id.clone()),
                approval_limit: 10,
                approval_target_bytes: None,
                recent_task_limit: Some(10),
            },
            |_| {},
        )
        .unwrap();
    assert!(snapshot.recent_tasks.unwrap().is_empty());
    let checkpoint = events.cursor();
    let expected_body = "Offline workbench acceptance report\n";
    let request =
        json!({"sessionId":session.id,"clientRequestId":"headless-report","input":expected_body});
    let task: TaskSummary = client.call("task/startSummary", &request, |_| {}).unwrap();
    let waiting = wait_for_status(&mut client, &task.id, TaskStatus::WaitingApproval);
    let approval = waiting.pending_approval.unwrap();
    // Simulate disconnect after submission while the UI still has the old cursor.
    drop(client);
    drop(server);

    let (server, mut client) = super::ag_ui_run::connect(&db);
    let mut feed = EventFeed::subscribe(
        &mut client,
        ReplayScope::Session(session.id),
        checkpoint,
        |_| {},
    )
    .unwrap();
    let recovered = drain(&mut feed, &mut client);
    assert!(recovered.iter().all(|event| event.task_id == task.id));
    assert!(
        recovered
            .iter()
            .any(|event| matches!(&event.data, EventKind::TaskStarted { .. }))
    );
    assert!(recovered.iter().any(|event| matches!(&event.data, EventKind::ApprovalRequested { approval: pending } if pending.id == approval.id)));
    assert!(feed.cursor() > checkpoint);
    let before_approval = feed.cursor();
    let repeated: TaskSummary = client.call("task/startSummary", &request, |_| {}).unwrap();
    assert_eq!(repeated.id, task.id);
    assert_eq!(repeated.status, TaskStatus::WaitingApproval);

    let mut decision = ApprovalRespondParams {
        task_id: task.id.clone(),
        approval_id: "unavailable-approval".into(),
        decision: ApprovalDecision::AllowOnce,
    };
    let unavailable: Result<Task, _> = client.call("approval/respond", &decision, |_| {});
    assert!(matches!(unavailable, Err(Error::Rpc { code: -32009, .. })));
    let existing: Vec<ArtifactSummary> = client
        .call("artifacts/list", &json!({"taskId":task.id}), |_| {})
        .unwrap();
    assert!(existing.is_empty());
    assert_eq!(client.event_watermark(|_| {}).unwrap(), before_approval);

    decision.approval_id = approval.id;
    let _: Task = client.call("approval/respond", &decision, |_| {}).unwrap();
    wait_for_status(&mut client, &task.id, TaskStatus::Completed);
    // All live watermarks were discarded. The gap notice must re-open a window
    // from the consumed cursor, including approval and the final artifact.
    assert!(
        feed.observe(Notification {
            method: "notifications/eventGap".into(),
            params: json!({})
        })
        .is_none()
    );
    let recovered = drain(&mut feed, &mut client);
    assert!(
        recovered
            .iter()
            .all(|event| event.sequence > before_approval && event.task_id == task.id)
    );
    assert_eq!(recovered.iter().filter(|event| matches!(&event.data, EventKind::ApprovalResolved { approval_id, decision: ApprovalDecision::AllowOnce } if *approval_id == decision.approval_id)).count(), 1);
    assert_eq!(
        recovered
            .iter()
            .filter(|event| matches!(event.data, EventKind::ToolStarted { .. }))
            .count(),
        1
    );
    let artifacts: Vec<_> = recovered
        .iter()
        .filter_map(|event| match &event.data {
            EventKind::ArtifactCreated { artifact_id } => Some(artifact_id),
            _ => None,
        })
        .collect();
    assert_eq!(artifacts.len(), 1);
    assert!(matches!(
        recovered.last().unwrap().data,
        EventKind::TaskCompleted { .. }
    ));
    let mut body = Vec::new();
    client
        .download_artifact_with_digest(
            &ArtifactGetParams {
                task_id: task.id,
                artifact_id: artifacts[0].clone(),
            },
            &mut body,
            |_| Ok(()),
            |_| {},
        )
        .unwrap();
    assert_eq!(body, expected_body.as_bytes());
    drop(client);
    drop(server);
    fs::remove_dir_all(root).unwrap();
}
