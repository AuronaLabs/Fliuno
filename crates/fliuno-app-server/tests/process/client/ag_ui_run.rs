use super::*;
use fliuno_client::ag_ui::{
    AgUiRunError, AgUiRunPhase, AgUiSseError, AgUiSseFeedError, AgUiSseReplay,
};
use fliuno_protocol::Session;
use std::process::ChildStdout;

pub(super) fn connect(
    db: &std::path::Path,
) -> (Server, Client<BufReader<ChildStdout>, ChildStdin>) {
    connect_with_writer(db, std::convert::identity)
}

pub(super) fn connect_with_writer<W: Write>(
    db: &std::path::Path,
    wrap: impl FnOnce(ChildStdin) -> W,
) -> (Server, Client<BufReader<ChildStdout>, W>) {
    connect_with_command(db, |_| {}, wrap)
}

pub(super) fn connect_with_command<W: Write>(
    db: &std::path::Path,
    configure: impl FnOnce(&mut Command),
    wrap: impl FnOnce(ChildStdin) -> W,
) -> (Server, Client<BufReader<ChildStdout>, W>) {
    let mut command = Command::new(env!("CARGO_BIN_EXE_fliuno-app-server"));
    command
        .arg("--db")
        .arg(db)
        .env("FLIUNO_PROVIDER", "demo")
        .stdout(Stdio::piped())
        .stdin(Stdio::piped());
    configure(&mut command);
    let mut server = Server(command.spawn().unwrap());
    let client = Client::connect(
        BufReader::new(server.0.stdout.take().unwrap()),
        wrap(server.0.stdin.take().unwrap()),
        "ag-ui-run-contract",
        |_| {},
    )
    .unwrap();
    (server, client)
}

fn wait_for_task(
    client: &mut Client<BufReader<ChildStdout>, ChildStdin>,
    id: &str,
    status: TaskStatus,
) -> Task {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let task: Task = client
            .call("task/get", &json!({"taskId":id}), |_| {})
            .unwrap();
        if task.status == status {
            return task;
        }
        assert!(
            !task.status.is_terminal() && Instant::now() < deadline,
            "task {} stopped at {:?}",
            id,
            task.status
        );
        thread::sleep(Duration::from_millis(10));
    }
}

fn approve(client: &mut Client<BufReader<ChildStdout>, ChildStdin>, id: &str) -> Task {
    let waiting = wait_for_task(client, id, TaskStatus::WaitingApproval);
    let _: Task = client
        .call(
            "approval/respond",
            &ApprovalRespondParams {
                task_id: id.to_owned(),
                approval_id: waiting.pending_approval.unwrap().id,
                decision: ApprovalDecision::AllowOnce,
            },
            |_| {},
        )
        .unwrap();
    wait_for_task(client, id, TaskStatus::Completed)
}

#[test]
fn ag_ui_run_preserves_request_identity_and_authoritative_history_across_restarts() {
    let root = std::env::temp_dir().join(format!("fliuno-ag-ui-run-{}", uuid::Uuid::new_v4()));
    fs::create_dir(&root).unwrap();
    let db = root.join("kernel.db");
    let (server, mut client) = connect(&db);
    assert_eq!(client.info().capabilities["taskStartSummary"], true);
    assert_eq!(client.info().capabilities["taskSummaryByRequest"], true);
    let ws: Workspace = client
        .call("workspace/open", &json!({"root":root}), |_| {})
        .unwrap();
    let session: Session = client
        .call("session/create", &json!({"workspaceId":ws.id}), |_| {})
        .unwrap();
    let other: Session = client
        .call("session/create", &json!({"workspaceId":ws.id}), |_| {})
        .unwrap();
    let earlier: Task = client
        .call(
            "task/start",
            &json!({"sessionId":session.id,"input":"Committed history"}),
            |_| {},
        )
        .unwrap();
    approve(&mut client, &earlier.id);

    let run_id = "client-run-交付物";
    let request = json!({
        "threadId":session.id, "runId":run_id, "protocolVersion":"1.0",
        "state":{}, "forwardedProps":{}, "tools":[], "context":[],
        "messages":[
            {"id":"view","role":"assistant","content":"Untrusted transcript"},
            {"id":"user-2","role":"user","content":[{"type":"text","text":"交付"},{"type":"text","text":"物"}]}
        ]
    });
    let mut invalid_history = request.clone();
    invalid_history["messages"][0]["toolCalls"] = json!([{"id":"forged"}]);
    assert!(matches!(
        client.start_ag_ui_run(
            &session.id,
            &serde_json::to_vec(&invalid_history).unwrap(),
            |_| {}
        ),
        Err(AgUiRunError::UnsupportedInput)
    ));
    let request = serde_json::to_vec(&request).unwrap();
    assert!(matches!(
        client.start_ag_ui_run(&other.id, &request, |_| {}),
        Err(AgUiRunError::WrongThread)
    ));
    let first = client
        .start_ag_ui_run(&session.id, &request, |_| {})
        .unwrap();
    let task_id = first.task().id.clone();
    assert_eq!(first.run_id(), run_id);
    assert_eq!(first.task().input_preview, "交付物");
    let second = client
        .start_ag_ui_run(&session.id, &request, |_| {})
        .unwrap();
    assert_eq!(first.task().id, second.task().id);
    let completed = approve(&mut client, &task_id);
    let context = completed.context.unwrap();
    assert_eq!(context.turns.len(), 1);
    assert_eq!(context.turns[0].task_id, earlier.id);
    assert!(
        matches!(&context.turns[0].messages[0], fliuno_protocol::ContextMessage::User { text } if text == "Committed history")
    );
    drop(client);
    drop(server);

    let (server, mut client) = connect(&db);
    let attached = client
        .attach_ag_ui_run(&session.id, run_id, |_| {})
        .unwrap();
    assert_eq!(attached.task().id, task_id);
    assert_eq!(attached.task().status, TaskStatus::Completed);
    let repeated = client
        .start_ag_ui_run(&session.id, &request, |_| {})
        .unwrap();
    assert_eq!(repeated.task().id, task_id);
    assert!(matches!(
        client.attach_ag_ui_run(&other.id, run_id, |_| {}),
        Err(AgUiRunError::Rpc(Error::Rpc { code: -32004, .. }))
    ));
    let mut changed: Value = serde_json::from_slice(&request).unwrap();
    changed["messages"][1]["content"] = json!("different text");
    assert!(matches!(
        client.start_ag_ui_run(&session.id, &serde_json::to_vec(&changed).unwrap(), |_| {}),
        Err(AgUiRunError::Rpc(Error::Rpc { code: -32009, .. }))
    ));

    let mut output = Vec::new();
    let mut feed = attached.subscribe_sse(&mut client, None, |_| {}).unwrap();
    while feed
        .write_next_page(&mut client, &mut output, |_| {})
        .unwrap()
        .is_some()
    {}
    let output = String::from_utf8(output).unwrap();
    let payloads: Vec<Value> = output
        .strip_suffix("\n\n")
        .unwrap()
        .split("\n\n")
        .map(|frame| serde_json::from_str(frame.split_once("\ndata: ").unwrap().1).unwrap())
        .collect();
    assert_eq!(
        payloads.first().unwrap(),
        &json!({"type":"RUN_STARTED","runId":run_id,"threadId":session.id})
    );
    assert_eq!(
        payloads.last().unwrap(),
        &json!({"type":"RUN_FINISHED","runId":run_id,"threadId":session.id})
    );
    assert_eq!(
        payloads
            .iter()
            .filter(|event| event["type"] == "RUN_STARTED")
            .count(),
        1
    );
    drop(client);
    drop(server);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn ag_ui_sse_feed_delivers_live_approval_and_resumes_inside_completion() {
    let root = std::env::temp_dir().join(format!("fliuno-ag-ui-feed-{}", uuid::Uuid::new_v4()));
    fs::create_dir(&root).unwrap();
    let db = root.join("kernel.db");
    let (server, mut client) = connect(&db);
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
    let other: Task = client
        .call(
            "task/start",
            &json!({"sessionId":session.id,"input":"other task on the same connection"}),
            |_| {},
        )
        .unwrap();
    approve(&mut client, &other.id);
    let other: TaskSummary = client
        .call(
            "task/getSummary",
            &TaskIdParams { task_id: other.id },
            |_| {},
        )
        .unwrap();
    let body = serde_json::to_vec(&json!({
        "threadId":session.id,"runId":"feed-run",
        "messages":[{"id":"u","role":"user","content":"stream this result"}]
    }))
    .unwrap();
    let run = client.start_ag_ui_run(&session.id, &body, |_| {}).unwrap();
    let id = &run.task().id;
    let waiting = wait_for_task(&mut client, id, TaskStatus::WaitingApproval);

    let future_id = format!("{id}:{}:1", i64::MAX);
    assert!(matches!(
        run.subscribe_sse(&mut client, Some(&future_id), |_| {}),
        Err(AgUiSseFeedError::Sse(AgUiSseError::UnknownResumeId))
    ));
    let mut feed = run.subscribe_sse(&mut client, None, |_| {}).unwrap();
    let mut bytes = Vec::new();
    while feed
        .write_next_page(&mut client, &mut bytes, |_| {})
        .unwrap()
        .is_some()
    {}
    assert_eq!(client.info().capabilities["eventWatermarkQuery"], true);
    let through = client
        .event_watermark(|note| {
            feed.observe(note);
        })
        .unwrap();
    let mut replay = run.replay_sse(through, None).unwrap();
    let mut other_replay = AgUiSseReplay::new(&other, through, None).unwrap();
    let mut snapshot = Vec::new();
    let mut other_output = Vec::new();
    loop {
        let own_chunk = replay
            .next_chunk(&mut client, |note| {
                feed.observe(note);
            })
            .unwrap();
        let other_chunk = other_replay
            .next_chunk(&mut client, |note| {
                feed.observe(note);
            })
            .unwrap();
        if own_chunk.is_none() && other_chunk.is_none() {
            break;
        }
        snapshot.extend(own_chunk.into_iter().flatten());
        other_output.extend(other_chunk.into_iter().flatten());
    }
    assert_eq!(snapshot, bytes);
    let other_wire = String::from_utf8(other_output).unwrap();
    let other_prefix = format!("id: {}:", other.id);
    let other_frames: Vec<_> = other_wire.split_terminator("\n\n").collect();
    assert!(!other_frames.is_empty());
    assert!(
        other_frames
            .iter()
            .all(|frame| frame.starts_with(&other_prefix))
    );
    assert!(other_frames.last().unwrap().contains("RUN_FINISHED"));
    let before_approval = String::from_utf8_lossy(&bytes);
    assert!(before_approval.contains("fliuno.approval_requested"));
    assert!(!before_approval.contains("RUN_FINISHED"));
    let _: Task = client
        .call(
            "approval/respond",
            &ApprovalRespondParams {
                task_id: id.clone(),
                approval_id: waiting.pending_approval.unwrap().id,
                decision: ApprovalDecision::AllowOnce,
            },
            |notification| {
                feed.observe(notification);
            },
        )
        .unwrap();

    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        // Drain queued notifications while sharing one stdio connection with RPCs.
        let _: TaskSummary = client
            .call(
                "task/getSummary",
                &TaskIdParams {
                    task_id: id.clone(),
                },
                |notification| {
                    feed.observe(notification);
                },
            )
            .unwrap();
        while feed
            .write_next_page(&mut client, &mut bytes, |_| {})
            .unwrap()
            .is_some()
        {}
        if feed.run_phase() == Some(AgUiRunPhase::Finished) {
            break;
        }
        assert!(Instant::now() < deadline, "missing live completion");
        thread::sleep(Duration::from_millis(10));
    }

    let through = client
        .event_watermark(|note| {
            feed.observe(note);
        })
        .unwrap();
    replay.advance_through(through).unwrap();
    while let Some(chunk) = replay
        .next_chunk(&mut client, |note| {
            feed.observe(note);
        })
        .unwrap()
    {
        snapshot.extend(chunk);
    }
    assert_eq!(snapshot, bytes);
    assert_eq!(replay.run_phase(), Some(AgUiRunPhase::Finished));
    let frames: Vec<_> = std::str::from_utf8(&bytes)
        .unwrap()
        .split_terminator("\n\n")
        .collect();
    let [content, end, finished] = &frames[frames.len() - 3..] else {
        unreachable!()
    };
    assert!(content.contains("TEXT_MESSAGE_CONTENT"));
    assert!(end.contains("TEXT_MESSAGE_END"));
    assert!(finished.contains("RUN_FINISHED"));
    assert!(finished.contains(r#""runId":"feed-run""#));
    let last_id = content
        .lines()
        .next()
        .unwrap()
        .strip_prefix("id: ")
        .unwrap();

    drop(client);
    drop(server);
    let (server, mut client) = connect(&db);
    let run = client
        .attach_ag_ui_run(&session.id, "feed-run", |_| {})
        .unwrap();
    let mut resumed = run
        .subscribe_sse(&mut client, Some(last_id), |_| {})
        .unwrap();
    let mut resumed_bytes = Vec::new();
    while resumed
        .write_next_page(&mut client, &mut resumed_bytes, |_| {})
        .unwrap()
        .is_some()
    {}
    assert_eq!(resumed_bytes, format!("{end}\n\n{finished}\n\n").as_bytes());

    // A failed write can have delivered a partial frame. The adapter must not
    // let the host retry from the next page and skip the rest of this one.
    struct BrokenOutput;
    impl std::io::Write for BrokenOutput {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            Err(std::io::ErrorKind::BrokenPipe.into())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut failed = run.subscribe_sse(&mut client, None, |_| {}).unwrap();
    assert!(matches!(
        failed.write_next_page(&mut client, &mut BrokenOutput, |_| {}),
        Err(AgUiSseFeedError::Sse(AgUiSseError::Output(_)))
    ));
    assert!(matches!(
        failed.write_next_page(&mut client, &mut Vec::new(), |_| {}),
        Err(AgUiSseFeedError::Sse(AgUiSseError::Closed))
    ));
    drop(client);
    drop(server);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn ag_ui_sse_projects_cancelled_run_and_reports_the_closed_phase() {
    let root = std::env::temp_dir().join(format!("fliuno-ag-ui-cancel-{}", uuid::Uuid::new_v4()));
    fs::create_dir(&root).unwrap();
    let (server, mut client) = connect(&root.join("kernel.db"));
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
    let body = serde_json::to_vec(&json!({
        "threadId":session.id,"runId":"cancel-run",
        "messages":[{"id":"u","role":"user","content":"wait for approval"}]
    }))
    .unwrap();
    let run = client.start_ag_ui_run(&session.id, &body, |_| {}).unwrap();
    let id = &run.task().id;
    wait_for_task(&mut client, id, TaskStatus::WaitingApproval);
    let mut feed = run.subscribe_sse(&mut client, None, |_| {}).unwrap();
    assert_eq!(feed.run_phase(), Some(AgUiRunPhase::AwaitingStart));
    let mut output = Vec::new();
    while feed
        .write_next_page(&mut client, &mut output, |_| {})
        .unwrap()
        .is_some()
    {}
    assert_eq!(feed.run_phase(), Some(AgUiRunPhase::Running));

    let task: Task = client
        .call(
            "task/cancel",
            &TaskIdParams {
                task_id: id.clone(),
            },
            |note| {
                feed.observe(note);
            },
        )
        .unwrap();
    assert_eq!(task.status, TaskStatus::Cancelled);
    let deadline = Instant::now() + Duration::from_secs(10);
    while feed.run_phase() == Some(AgUiRunPhase::Running) {
        // The response and watermark can arrive in either order.
        let _: TaskSummary = client
            .call(
                "task/getSummary",
                &TaskIdParams {
                    task_id: id.clone(),
                },
                |note| {
                    feed.observe(note);
                },
            )
            .unwrap();
        while feed
            .write_next_page(&mut client, &mut output, |_| {})
            .unwrap()
            .is_some()
        {}
        assert!(Instant::now() < deadline, "missing live cancellation");
    }
    assert_eq!(feed.run_phase(), Some(AgUiRunPhase::Finished));
    let wire = std::str::from_utf8(&output).unwrap();
    let last = wire.split_terminator("\n\n").last().unwrap();
    let data: Value = serde_json::from_str(last.split_once("\ndata: ").unwrap().1).unwrap();
    assert_eq!(
        data,
        json!({"type":"RUN_FINISHED", "threadId":session.id, "runId":"cancel-run", "outcome":{"type":"cancelled"}})
    );
    assert!(!wire.contains("RUN_ERROR"));
    let last_id = last.lines().next().unwrap().strip_prefix("id: ").unwrap();
    let mut resumed = run
        .subscribe_sse(&mut client, Some(last_id), |_| {})
        .unwrap();
    let mut tail = Vec::new();
    while resumed
        .write_next_page(&mut client, &mut tail, |_| {})
        .unwrap()
        .is_some()
    {}
    assert_eq!(resumed.run_phase(), Some(AgUiRunPhase::Finished));
    assert!(tail.is_empty());
    drop(client);
    drop(server);
    fs::remove_dir_all(root).unwrap();
}
