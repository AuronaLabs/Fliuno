use super::*;
use fliuno_client::{EventFeed, ag_ui::AgUiRunError};
use fliuno_protocol::Session;
use std::process::ChildStdout;

fn connect(db: &std::path::Path) -> (Server, Client<BufReader<ChildStdout>, ChildStdin>) {
    let mut server = Server(
        Command::new(env!("CARGO_BIN_EXE_fliuno-app-server"))
            .arg("--db")
            .arg(db)
            .env("FLIUNO_PROVIDER", "demo")
            .stdout(Stdio::piped())
            .stdin(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let client = Client::connect(
        BufReader::new(server.0.stdout.take().unwrap()),
        server.0.stdin.take().unwrap(),
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
    let mut stream = attached.sse(None).unwrap();
    let mut feed =
        EventFeed::subscribe(&mut client, ReplayScope::Task(task_id), 0, |_| {}).unwrap();
    while let Some(page) = feed.next_page(&mut client, 2, Some(4096), |_| {}).unwrap() {
        for event in page {
            stream.write_event(event, &mut output).unwrap();
        }
    }
    stream.finish_replay().unwrap();
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
