//! Tests the actual executable, wire framing, and on-disk restart semantics.
use serde_json::{Value, json};
#[path = "../../fliuno-mcp/tests/support/http.rs"]
mod http_wire;
use std::{
    fs,
    io::{BufRead, BufReader, Read, Write},
    net::{TcpListener, TcpStream},
    path::PathBuf,
    process::{Child, ChildStdin, Command, Stdio},
    sync::mpsc::{self, Receiver},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

#[path = "process/a2a.rs"]
mod a2a;
#[path = "process/approvals.rs"]
mod approvals;
#[path = "process/artifacts.rs"]
mod artifacts;
#[path = "process/context.rs"]
mod context;
#[path = "process/event_stream.rs"]
mod event_stream;
#[cfg(feature = "test-fixtures")]
#[path = "process/execution.rs"]
mod execution;
#[path = "process/model_budget.rs"]
mod model_budget;
#[path = "process/sessions.rs"]
mod sessions;
#[path = "process/task_status.rs"]
mod task_status;
#[cfg(feature = "test-fixtures")]
#[path = "process/terminal.rs"]
mod terminal;
#[path = "process/workspace_search.rs"]
mod workspace_search;

struct Peer {
    child: Child,
    input: ChildStdin,
    output: Receiver<Value>,
    reader: Option<thread::JoinHandle<()>>,
    id: u64,
    notifications: Vec<Value>,
}
impl Peer {
    fn start(db: &PathBuf) -> Self {
        Self::start_with_model_fixture(db, None)
    }

    fn start_with_model_fixture(db: &PathBuf, base: Option<&str>) -> Self {
        Self::start_with_remote_tools(db, base, None)
    }

    fn start_with_remote_tools(db: &PathBuf, base: Option<&str>, mcp: Option<&PathBuf>) -> Self {
        Self::start_with_configuration(db, base, mcp, None)
    }

    fn start_with_configuration(
        db: &PathBuf,
        base: Option<&str>,
        mcp: Option<&PathBuf>,
        execution: Option<&PathBuf>,
    ) -> Self {
        Self::start_with_all_configuration(db, base, mcp, execution, None)
    }
    fn start_with_all_configuration(
        db: &PathBuf,
        base: Option<&str>,
        mcp: Option<&PathBuf>,
        execution: Option<&PathBuf>,
        terminal: Option<&PathBuf>,
    ) -> Self {
        Self::start_with_agent_configuration(db, base, mcp, execution, terminal, None)
    }
    fn start_with_a2a(db: &PathBuf, config: &PathBuf) -> Self {
        Self::start_with_agent_configuration(db, None, None, None, None, Some(config))
    }
    fn start_with_agent_configuration(
        db: &PathBuf,
        base: Option<&str>,
        mcp: Option<&PathBuf>,
        execution: Option<&PathBuf>,
        terminal: Option<&PathBuf>,
        a2a: Option<&PathBuf>,
    ) -> Self {
        let mut command = Command::new(env!("CARGO_BIN_EXE_fliuno-app-server"));
        command
            .arg("--db")
            .arg(db)
            .env("FLIUNO_PROVIDER", "demo")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        if let Some(base) = base {
            command
                .env("FLIUNO_PROVIDER", "openai")
                .env("FLIUNO_MODEL", "fixture-model")
                .env("OPENAI_API_KEY", "fixture-key")
                .env("OPENAI_BASE_URL", base);
        }
        if let Some(config) = mcp {
            command
                .arg("--mcp-config")
                .arg(config)
                .env("FLIUNO_TEST_MCP_TOKEN", "fixture-mcp-private")
                .env("FLIUNO_TEST_MCP_VENDOR", "fixture-vendor-private");
        }
        if let Some(config) = execution {
            command
                .arg("--exec-config")
                .arg(config)
                .env("FLIUNO_EXEC_SECRET", "fixture-not-inherited");
        }
        if let Some(config) = terminal {
            command
                .arg("--terminal-config")
                .arg(config)
                .env("FLIUNO_TERMINAL_SECRET", "must-not-leak");
        }
        if let Some(config) = a2a {
            command
                .arg("--a2a-config")
                .arg(config)
                .env("FLIUNO_TEST_A2A_TOKEN", "fixture-a2a-private");
        }
        let mut peer = Self::from_child(command.spawn().unwrap());
        let init = peer.call(
            "initialize",
            json!({"protocolVersion":"0.1","clientName":"process-test"}),
        );
        assert_eq!(init["capabilities"]["sessionContext"], true);
        assert_eq!(init["capabilities"]["sessionTitles"], true);
        assert_eq!(init["capabilities"]["workspaceById"], true);
        assert_eq!(init["capabilities"]["taskEventSubscription"], true);
        assert_eq!(init["capabilities"]["taskSummaryById"], true);
        assert_eq!(init["capabilities"]["taskSummaryStatusFilter"], true);
        assert_eq!(init["capabilities"]["workspaceTaskSummaries"], true);
        assert_eq!(init["capabilities"]["sessionArchiving"], true);
        assert_eq!(init["capabilities"]["sessionArchiveBatch"], true);
        assert_eq!(init["capabilities"]["maxSessionArchiveBatch"], 32);
        assert_eq!(init["capabilities"]["sessionCreateIdempotency"], true);
        assert_eq!(init["capabilities"]["contextTaskSelection"], true);
        assert_eq!(init["capabilities"]["contextFitRecent"], true);
        assert_eq!(init["capabilities"]["contextConversationDetail"], true);
        assert_eq!(init["capabilities"]["contextAtSequence"], true);
        assert_eq!(init["capabilities"]["maxContextCandidates"], 128);
        assert_eq!(init["capabilities"]["registrationAuthority"], true);
        assert_eq!(init["capabilities"]["pendingApprovalInbox"], true);
        assert_eq!(
            init["capabilities"]["pendingApprovalListTargetBytes"],
            8_388_608
        );
        assert_eq!(init["capabilities"]["pendingApprovalDetails"], true);
        assert_eq!(init["capabilities"]["artifactSummaries"], true);
        assert_eq!(init["capabilities"]["taskListTargetBytes"], 8_388_608);
        assert_eq!(
            init["capabilities"]["taskSummaryListTargetBytes"],
            8_388_608
        );
        assert_eq!(init["capabilities"]["agentCapabilitiesById"], true);
        assert_eq!(init["capabilities"]["artifactSummaryById"], true);
        assert_eq!(init["capabilities"]["artifactByteReads"], true);
        assert_eq!(init["capabilities"]["binaryArtifacts"], true);
        assert_eq!(init["capabilities"]["maxBinaryArtifactBytes"], 262_144);
        assert_eq!(init["capabilities"]["maxArtifactReadBytes"], 65_536);
        assert_eq!(init["capabilities"]["eventResubscribe"], true);
        assert_eq!(init["capabilities"]["sessionEventReplay"], true);
        assert_eq!(init["capabilities"]["eventSubscriptionScope"], true);
        assert_eq!(init["capabilities"]["eventBySequence"], true);
        assert_eq!(
            init["capabilities"]["eventReplayTargetBytes"],
            8 * 1024 * 1024
        );
        if base.is_none() {
            assert!(init["capabilities"]["modelBudget"].is_null());
        }
        assert_eq!(init["capabilities"]["remoteTaskChecks"], a2a.is_some());
        assert_eq!(
            init["capabilities"]["remoteTaskContinuation"],
            a2a.is_some()
        );
        assert_eq!(
            init["capabilities"]["modelProgressNotifications"],
            base.is_some()
        );
        assert_eq!(init["capabilities"]["maxContextTurns"], 32);
        assert_eq!(init["capabilities"]["maxContextBytes"], 262144);
        assert_eq!(
            init["capabilities"]["processExecution"],
            execution.is_some()
        );
        assert_eq!(init["capabilities"]["terminalSessions"], terminal.is_some());
        assert_eq!(
            init["capabilities"]["terminalBatchArchiving"],
            terminal.is_some()
        );
        assert_eq!(init["capabilities"]["maxTerminalArchiveBatch"], 32);
        assert_eq!(
            init["capabilities"]["terminalArchiving"],
            terminal.is_some()
        );
        peer
    }
    fn from_child(mut child: Child) -> Self {
        let input = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let (output_tx, output) = mpsc::channel();
        let reader = thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                let Ok(value) = serde_json::from_str(&line) else {
                    break;
                };
                if output_tx.send(value).is_err() {
                    break;
                }
            }
        });
        Self {
            child,
            input,
            output,
            reader: Some(reader),
            id: 0,
            notifications: Vec::new(),
        }
    }
    fn request(&mut self, method: &str, params: Value) -> Value {
        self.id += 1;
        writeln!(
            self.input,
            "{}",
            json!({"jsonrpc":"2.0","id":self.id,"method":method,"params":params})
        )
        .unwrap();
        self.input.flush().unwrap();
        // The first handshake also waits for the process to load and open or
        // migrate SQLite. Keep the shorter deadline for subsequent requests.
        let response_timeout = if method == "initialize" && self.id == 1 {
            Duration::from_secs(15)
        } else {
            Duration::from_secs(5)
        };
        let deadline = Instant::now() + response_timeout;
        loop {
            let response = self
                .output
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .unwrap_or_else(|error| panic!("RPC {method} did not respond: {error}"));
            if response.get("method").is_some() && response.get("id").is_none() {
                self.notifications.push(response);
                continue;
            }
            assert_eq!(response["id"], self.id);
            return response;
        }
    }

    fn call(&mut self, method: &str, params: Value) -> Value {
        let response = self.request(method, params);
        assert!(response.get("error").is_none(), "{response}");
        response["result"].clone()
    }

    fn wait_for_status(&mut self, task_id: &str, expected: &str) -> Value {
        // Native workers may need several seconds to start on a busy runner.
        // This is a state/ordering check; responsive RPC/cancellation has its
        // own deadlines. Keep this wait bounded by wall time, with evidence.
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let task = self.call("task/get", json!({"taskId":task_id}));
            if task["status"] == expected {
                return task;
            }
            assert!(
                !matches!(
                    task["status"].as_str(),
                    Some("failed" | "cancelled" | "interrupted" | "completed")
                ),
                "task stopped before {expected}: {task}"
            );
            assert!(
                Instant::now() < deadline,
                "task {task_id} did not reach status {expected}; last snapshot: {task}"
            );
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn wait_for_event(&mut self, task_id: &str, event_type: &str) -> Value {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(event) = self.notifications.iter().find(|notification| {
                notification["method"] == "notifications/event"
                    && notification["params"]["event"]["taskId"] == task_id
                    && notification["params"]["event"]["data"]["type"] == event_type
            }) {
                return event.clone();
            }
            let notification = self
                .output
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .unwrap_or_else(|error| panic!("event {event_type} was not delivered: {error}"));
            self.notifications.push(notification);
        }
    }

    fn wait_for_model_progress(&mut self, task_id: &str) -> Value {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(progress) = self.notifications.iter().find(|notification| {
                notification["method"] == "notifications/modelProgress"
                    && notification["params"]["progress"]["taskId"] == task_id
            }) {
                return progress.clone();
            }
            let notification = self
                .output
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .unwrap_or_else(|error| panic!("model progress was not delivered: {error}"));
            self.notifications.push(notification);
        }
    }
}
impl Drop for Peer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

fn accept_request(listener: &TcpListener) -> TcpStream {
    listener.set_nonblocking(true).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut stream = loop {
        match listener.accept() {
            Ok((stream, _)) => break stream,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                assert!(
                    Instant::now() < deadline,
                    "provider did not connect to fixture"
                );
                thread::sleep(Duration::from_millis(5));
            }
            Err(error) => panic!("fixture accept failed: {error}"),
        }
    };
    stream.set_nonblocking(false).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    stream
        .set_write_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut reader = BufReader::new(&mut stream);
    let mut content_length = None;
    loop {
        let mut line = String::new();
        assert!(reader.read_line(&mut line).unwrap() > 0);
        if line == "\r\n" {
            break;
        }
        if let Some((key, value)) = line.split_once(':')
            && key.eq_ignore_ascii_case("content-length")
        {
            content_length = Some(value.trim().parse::<usize>().unwrap());
        }
    }
    let length = content_length.unwrap();
    assert!(length < 1024 * 1024);
    reader.read_exact(&mut vec![0; length]).unwrap();
    stream
}

fn complete_request(mut stream: TcpStream, text: &str) {
    let body = json!({"status":"completed","output":[{
        "type":"message","content":[{"type":"output_text","text":text}]
    }]})
    .to_string();
    write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
}

fn write_sse_chunk(stream: &mut TcpStream, event: Value) {
    let frame = format!("data: {event}\n\n");
    write!(stream, "{:x}\r\n{frame}\r\n", frame.len()).unwrap();
    stream.flush().unwrap();
}

#[test]
fn openai_stream_progress_reaches_stdio_and_final_result_stays_durable() {
    let root = std::env::temp_dir().join(format!(
        "fliuno-progress-rpc-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir(&root).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}/v1/", listener.local_addr().unwrap());
    let mut peer = Peer::start_with_model_fixture(&root.join("kernel.db"), Some(&base));
    let workspace = peer.call("workspace/open", json!({"root":root}));
    let session = peer.call("session/create", json!({"workspaceId":workspace["id"]}));
    let task = peer.call(
        "task/start",
        json!({"sessionId":session["id"],"input":"stream"}),
    );
    let mut stream = accept_request(&listener);
    write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n").unwrap();
    write_sse_chunk(
        &mut stream,
        json!({"type":"response.output_text.delta","delta":"Hello "}),
    );
    let progress = peer.wait_for_model_progress(task["id"].as_str().unwrap());
    assert_eq!(progress["params"]["progress"]["type"], "text_delta");
    assert_eq!(progress["params"]["progress"]["step"], 1);
    assert_eq!(progress["params"]["progress"]["text"], "Hello ");
    let pending = peer.call("task/get", json!({"taskId":task["id"]}));
    assert_eq!(pending["status"], "running");
    assert_eq!(pending["modelOutcomeUnknown"], true);
    assert_eq!(pending["output"], Value::Null);
    assert!(
        pending["messages"]
            .as_array()
            .unwrap()
            .iter()
            .all(|message| message["role"] != "assistant")
    );
    write_sse_chunk(
        &mut stream,
        json!({"type":"response.output_text.delta","delta":"world"}),
    );
    write_sse_chunk(
        &mut stream,
        json!({"type":"response.completed","response":{"status":"completed","output":[{"type":"message","content":[{"type":"output_text","text":"Hello world"}]}]}}),
    );
    stream.write_all(b"0\r\n\r\n").unwrap();
    stream.flush().unwrap();
    let completed = peer.wait_for_status(task["id"].as_str().unwrap(), "completed");
    assert_eq!(completed["output"], "Hello world");
    let deadline = Instant::now() + Duration::from_secs(5);
    while peer
        .notifications
        .iter()
        .filter(|notification| {
            notification["method"] == "notifications/modelProgress"
                && notification["params"]["progress"]["taskId"] == task["id"]
        })
        .count()
        < 2
    {
        peer.notifications.push(
            peer.output
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .expect("second model preview was not delivered"),
        );
    }
    let streamed_text = peer
        .notifications
        .iter()
        .filter(|notification| {
            notification["method"] == "notifications/modelProgress"
                && notification["params"]["progress"]["taskId"] == task["id"]
                && notification["params"]["progress"]["type"] == "text_delta"
        })
        .map(|notification| notification["params"]["progress"]["text"].as_str().unwrap())
        .collect::<String>();
    assert_eq!(streamed_text, "Hello world");
    let events = peer.call("events/list", json!({"taskId":task["id"]}));
    assert_eq!(events["events"].as_array().unwrap().len(), 3);
    drop(peer);
    drop(stream);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn cancelling_task_aborts_provider_http_request_and_stdio_stays_responsive() {
    let root = std::env::temp_dir().join(format!(
        "fliuno-slow-http-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir(&root).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}/v1/", listener.local_addr().unwrap());
    let mut peer = Peer::start_with_model_fixture(&root.join("kernel.db"), Some(&base));
    let ws = peer.call("workspace/open", json!({"root":root}));
    let session = peer.call("session/create", json!({"workspaceId":ws["id"]}));
    let first = peer.call(
        "task/start",
        json!({"sessionId":session["id"],"input":"slow request"}),
    );
    let mut first_request = accept_request(&listener);
    assert_eq!(
        peer.call("task/get", json!({"taskId":first["id"]}))["status"],
        "running"
    );
    assert_eq!(
        peer.call("task/cancel", json!({"taskId":first["id"]}))["status"],
        "cancelled"
    );
    let second = peer.call(
        "task/start",
        json!({"sessionId":session["id"],"input":"next request"}),
    );
    assert_eq!(second["modelSteps"], 0);
    let mut closed = [0; 1];
    assert_eq!(
        first_request.read(&mut closed).unwrap(),
        0,
        "cancel should close the in-flight provider connection"
    );
    let second_request = accept_request(&listener);
    complete_request(second_request, "next task completed");
    let done = peer.wait_for_status(second["id"].as_str().unwrap(), "completed");
    assert_eq!(done["output"], "next task completed");
    let cancelled = peer.call("task/get", json!({"taskId":first["id"]}));
    assert_eq!(cancelled["status"], "cancelled");
    assert_eq!(cancelled["output"], Value::Null);
    let events = peer.call("events/list", json!({"taskId":first["id"]}));
    assert!(
        !events["events"]
            .as_array()
            .unwrap()
            .iter()
            .any(|event| event["data"]["type"] == "task_completed")
    );
    drop(peer);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn cancelling_task_interrupts_provider_response_body_read() {
    let root = std::env::temp_dir().join(format!(
        "fliuno-http-body-cancel-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir(&root).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}/v1/", listener.local_addr().unwrap());
    let mut peer = Peer::start_with_model_fixture(&root.join("kernel.db"), Some(&base));
    let ws = peer.call("workspace/open", json!({"root":root}));
    let session = peer.call("session/create", json!({"workspaceId":ws["id"]}));
    let task = peer.call(
        "task/start",
        json!({"sessionId":session["id"],"input":"wait for body"}),
    );
    let mut request = accept_request(&listener);
    write!(
        request,
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 1000\r\n\r\n{{"
    )
    .unwrap();
    request.flush().unwrap();
    thread::sleep(Duration::from_millis(50));
    assert_eq!(
        peer.call("task/get", json!({"taskId":task["id"]}))["status"],
        "running"
    );
    assert_eq!(
        peer.call("task/cancel", json!({"taskId":task["id"]}))["status"],
        "cancelled"
    );
    let next = peer.call(
        "task/start",
        json!({"sessionId":session["id"],"input":"next request"}),
    );
    let next_request = accept_request(&listener);
    complete_request(next_request, "worker advanced after body cancellation");
    assert_eq!(
        peer.wait_for_status(next["id"].as_str().unwrap(), "completed")["output"],
        "worker advanced after body cancellation"
    );
    assert_eq!(
        peer.call("task/get", json!({"taskId":task["id"]}))["status"],
        "cancelled"
    );
    drop(request);
    drop(peer);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn stdio_approval_restart_and_artifact_roundtrip() {
    let root = std::env::temp_dir().join(format!(
        "fliuno-process-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir(&root).unwrap();
    let db = root.join("kernel.db");
    let mut peer = Peer::start(&db);
    let ws = peer.call("workspace/open", json!({"root":root}));
    let session = peer.call("session/create", json!({"workspaceId":ws["id"]}));
    let task = peer.call(
        "task/start",
        json!({"sessionId":session["id"],"input":"中文交付物 ✓","clientRequestId":"desktop-send-1"}),
    );
    assert_eq!(task["status"], "running");
    let task = peer.wait_for_status(task["id"].as_str().unwrap(), "waiting_approval");
    let approval_event = peer.wait_for_event(task["id"].as_str().unwrap(), "approval_requested");
    assert_eq!(approval_event["params"]["event"]["sequence"], 3);
    let retry = peer.call(
        "task/start",
        json!({"sessionId":session["id"],"input":"中文交付物 ✓","clientRequestId":"desktop-send-1"}),
    );
    assert_eq!(retry["id"], task["id"]);
    assert_eq!(
        peer.call("task/list", json!({"sessionId":session["id"]}))["items"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        peer.call("artifacts/list", json!({"taskId":task["id"]})),
        json!([])
    );
    let summary_params = json!({"sessionId":session["id"],"limit":1});
    let task_params = json!({"taskId":task["id"]});
    assert_eq!(
        peer.request("task/getSummary", json!({"taskId":"unknown"}))["error"]["code"],
        -32004
    );
    let summaries = peer.call("task/listSummaries", summary_params.clone());
    assert_eq!(
        peer.call("task/getSummary", task_params.clone()),
        summaries["items"][0]
    );
    assert_eq!(
        summaries["items"],
        json!([{
            "id":task["id"],"sessionId":session["id"],"status":"waiting_approval",
            "backend":{"type":"model"},"inputPreview":"中文交付物 ✓","pendingApprovalCount":1,
            "modelOutcomeUnknown":false,"agentOutcomeUnknown":false
        }])
    );
    drop(peer); // Simulate a daemon crash while the user is reviewing approval.
    let mut peer = Peer::start(&db);
    let restored = peer.call("task/get", json!({"taskId":task["id"]}));
    assert_eq!(
        restored["pendingApproval"]["id"],
        task["pendingApproval"]["id"]
    );
    assert_eq!(
        peer.call("task/listSummaries", summary_params.clone()),
        summaries
    );
    let resumed=peer.call("approval/respond",json!({"taskId":task["id"],"approvalId":task["pendingApproval"]["id"],"decision":"allow_once"}));
    assert_eq!(resumed["status"], "running");
    let done = peer.wait_for_status(task["id"].as_str().unwrap(), "completed");
    assert_eq!(done["status"], "completed");
    let artifacts = peer.call("artifacts/list", json!({"taskId":task["id"]}));
    assert_eq!(artifacts.as_array().unwrap().len(), 1);
    assert_eq!(artifacts[0]["content"], "中文交付物 ✓");
    let done_page = peer.call("task/listSummaries", summary_params);
    assert_eq!(
        peer.call("task/getSummary", task_params),
        done_page["items"][0]
    );
    assert_eq!(done_page["nextAfter"], summaries["nextAfter"]);
    assert_eq!(done_page["items"][0]["status"], "completed");
    assert_eq!(done_page["items"][0]["pendingApprovalCount"], 0);
    let events = peer.call("events/list", json!({"taskId":task["id"],"limit":2}));
    assert_eq!(events["hasMore"], true);
    let rest = peer.call(
        "events/list",
        json!({"taskId":task["id"],"after":events["nextAfter"]}),
    );
    assert_eq!(
        rest["events"].as_array().unwrap().last().unwrap()["data"]["type"],
        "task_completed"
    );
    let all_events = peer.call("events/listAll", json!({"after":0,"limit":100}));
    let event_sequences: Vec<_> = all_events["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|event| event["sequence"].as_u64().unwrap())
        .collect();
    assert!(event_sequences.windows(2).all(|pair| pair[0] < pair[1]));
    assert_eq!(event_sequences.last(), Some(&9));
    drop(peer);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn schema_export_is_valid_json_and_needs_no_provider_configuration() {
    let output = Command::new(env!("CARGO_BIN_EXE_fliuno-app-server"))
        .arg("--schema")
        .env("FLIUNO_PROVIDER", "invalid")
        .output()
        .unwrap();
    assert!(output.status.success());
    let schema: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(schema["protocolVersion"], "0.1");
    assert!(schema["methods"]["task/start"].is_object());
}

#[test]
fn event_replay_byte_target_preserves_each_event_and_cursor_across_restart() {
    let root = std::env::temp_dir().join(format!(
        "fliuno-event-target-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir(&root).unwrap();
    let db = root.join("kernel.db");
    let mut peer = Peer::start(&db);
    let ws = peer.call("workspace/open", json!({"root":root}));
    let session = peer.call("session/create", json!({"workspaceId":ws["id"]}));
    let first = peer.call(
        "task/start",
        json!({"sessionId":session["id"],"input":"汉字\\\n".repeat(1024)}),
    );
    let first_id = first["id"].as_str().unwrap();
    peer.wait_for_status(first_id, "waiting_approval");
    let second = peer.call(
        "task/start",
        json!({"sessionId":session["id"],"input":"small"}),
    );
    peer.wait_for_status(second["id"].as_str().unwrap(), "waiting_approval");
    let full = peer.call("events/listAll", json!({}));
    let scoped = peer.call("events/list", json!({"taskId":first_id}));
    let first_page = peer.call("events/listAll", json!({"targetBytes":1024}));
    assert_eq!(first_page["hasMore"], true);
    assert!(serde_json::to_vec(&first_page["events"]).unwrap().len() <= 1024);
    drop(peer);

    let mut peer = Peer::start(&db);
    for (method, scope, expected, initial) in [
        (
            "events/listAll",
            json!({}),
            &full["events"],
            Some(first_page),
        ),
        (
            "events/list",
            json!({"taskId":first_id}),
            &scoped["events"],
            None,
        ),
    ] {
        let mut cursor = 0;
        let mut received = Vec::new();
        let mut large_event = false;
        let mut page = initial;
        loop {
            let response = page.take().unwrap_or_else(|| {
                let mut params = scope.clone();
                params["after"] = json!(cursor);
                params["targetBytes"] = json!(1024);
                peer.call(method, params)
            });
            let events = response["events"].as_array().unwrap();
            assert!(!events.is_empty());
            if serde_json::to_vec(events).unwrap().len() > 1024 {
                assert_eq!(events.len(), 1);
                large_event = true;
            }
            let next = response["nextAfter"].as_u64().unwrap();
            assert!(next > cursor);
            assert_eq!(events.last().unwrap()["sequence"], next);
            received.extend(events.iter().cloned());
            cursor = next;
            if response["hasMore"] == false {
                break;
            }
        }
        assert!(large_event);
        assert_eq!(json!(received), *expected);
    }
    assert_eq!(
        peer.request("events/listAll", json!({"targetBytes":1023}))["error"]["code"],
        -32602
    );
    assert_eq!(
        peer.request(
            "events/list",
            json!({"taskId":first_id,"targetBytes":8388609})
        )["error"]["code"],
        -32602
    );
    drop(peer);
    for entry in fs::read_dir(&root).unwrap() {
        fs::remove_file(entry.unwrap().path()).unwrap();
    }
    fs::remove_dir(root).unwrap();
}

#[test]
fn configured_model_budget_is_reported_without_contacting_the_provider() {
    for (provider, mode, reported) in [
        ("openai", "provider", "provider"),
        ("anthropic", "local", "json_bytes_estimate"),
    ] {
        let root = std::env::temp_dir().join(format!(
            "fliuno-model-budget-{}-{}-{provider}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
        ));
        fs::create_dir(&root).unwrap();
        let mut child = Command::new(env!("CARGO_BIN_EXE_fliuno-app-server"))
            .arg("--db")
            .arg(root.join("kernel.db"))
            .env("FLIUNO_PROVIDER", provider)
            .env("FLIUNO_MODEL", "fixture")
            .env("FLIUNO_CONTEXT_WINDOW_TOKENS", "32768")
            .env("FLIUNO_MAX_OUTPUT_TOKENS", "2048")
            .env("FLIUNO_CONTEXT_COUNT_MODE", mode)
            .env("OPENAI_BASE_URL", "http://127.0.0.1:1/")
            .env("ANTHROPIC_BASE_URL", "http://127.0.0.1:1/")
            .env("OPENAI_API_KEY", "fixture")
            .env("ANTHROPIC_API_KEY", "fixture")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        writeln!(
            child.stdin.take().unwrap(),
            "{}",
            json!({
                "jsonrpc":"2.0","id":1,"method":"initialize",
                "params":{"protocolVersion":"0.1","clientName":"budget-test"}
            })
        )
        .unwrap();
        let output = child.wait_with_output().unwrap();
        // Only remove regular files created in this test's fresh directory.
        for entry in fs::read_dir(&root).unwrap() {
            fs::remove_file(entry.unwrap().path()).unwrap();
        }
        fs::remove_dir(root).unwrap();
        assert!(output.status.success());
        let init: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(
            init["result"]["capabilities"]["modelBudget"],
            json!({
                "windowTokens":32768,"maxInputTokens":30720,"maxOutputTokens":2048,
                "countMode":reported,"source":"configured"
            })
        );
    }
}

#[test]
fn remote_mcp_http_approval_and_recovery_preserve_credentials_and_effect_identity() {
    use http_wire::{Fixture, Response, Version};
    for decision in ["allow_once", "deny", "unknown"] {
        let root = std::env::temp_dir().join(format!(
            "fliuno-remote-mcp-{}-{decision}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&root).unwrap();
        let fixture = Fixture::start(move |request| {
            if request.path == "/v1/responses" {
                assert_eq!(request.headers["authorization"], "Bearer fixture-key");
                assert!(!request.headers.contains_key("x-vendor-key"));
                let input = request.body["input"].as_array().unwrap();
                let result = if let Some(output) =
                    input.iter().find(|v| v["type"] == "function_call_output")
                {
                    assert_eq!(output["call_id"], "remote-call");
                    assert!(output["output"].as_str().unwrap().contains("远程工具内容"));
                    json!([{"type":"message","content":[{"type":"output_text","text":"远程任务完成"}]}])
                } else {
                    let name = request.body["tools"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .find_map(|t| {
                            t["name"]
                                .as_str()
                                .filter(|name| name.starts_with("mcp_remote_"))
                        })
                        .unwrap();
                    json!([{"type":"function_call","call_id":"remote-call","name":name,"arguments":"{\"text\":\"远程工具内容\"}"}])
                };
                return Response::json(200, json!({"status":"completed","output":result}));
            }
            assert_eq!(request.path, "/mcp");
            assert_eq!(
                request.headers["authorization"],
                "Bearer fixture-mcp-private"
            );
            assert_eq!(request.headers["x-vendor-key"], "fixture-vendor-private");
            if decision == "unknown" && request.body["method"] == "tools/call" {
                return Response::json(404, json!({"error":"private-remote-error-body"}));
            }
            http_wire::reply(request, Version::Modern)
        });
        let config = root.join("mcp.json");
        fs::write(&config, json!({"servers":[{"id":"remote","transport":"streamable_http","url":fixture.url("/mcp"),"bearerTokenEnv":"FLIUNO_TEST_MCP_TOKEN","headersEnv":{"X-Vendor-Key":"FLIUNO_TEST_MCP_VENDOR"},"timeoutMs":2000}]}).to_string()).unwrap();
        let db = root.join("kernel.db");
        let base = fixture.url("/v1/");
        let mut peer = Peer::start_with_remote_tools(&db, Some(&base), Some(&config));
        let workspace = peer.call("workspace/open", json!({"root":root}));
        let session = peer.call("session/create", json!({"workspaceId":workspace["id"]}));
        let task = peer.call("task/start", json!({"sessionId":session["id"],"input":"调用远程工具","clientRequestId":"remote-once"}));
        let waiting = peer.wait_for_status(task["id"].as_str().unwrap(), "waiting_approval");
        assert!(
            waiting["pendingApproval"]["call"]["name"]
                .as_str()
                .unwrap()
                .starts_with("mcp_remote_")
        );
        assert!(
            !fixture
                .requests()
                .iter()
                .any(|r| r.body["method"] == "tools/call")
        );
        peer.call("approval/respond", json!({"taskId":task["id"],"approvalId":waiting["pendingApproval"]["id"],"decision":if decision == "deny" {"deny"} else {"allow_once"}}));
        let expected = match decision {
            "allow_once" => "completed",
            "deny" => "failed",
            _ => "interrupted",
        };
        let done = peer.wait_for_status(task["id"].as_str().unwrap(), expected);
        if decision == "allow_once" {
            assert_eq!(done["output"], "远程任务完成");
        }
        if decision == "unknown" {
            assert!(done["inFlightTool"].is_object());
        }
        let events = peer.call("events/list", json!({"taskId":task["id"]}));
        let saved = format!("{done}{events}");
        for private in [
            "fixture-mcp-private",
            "fixture-vendor-private",
            "private-remote-error-body",
        ] {
            assert!(!saved.contains(private));
        }
        drop(peer);
        let mut peer = Peer::start_with_remote_tools(&db, Some(&base), Some(&config));
        assert_eq!(
            peer.call("task/get", json!({"taskId":task["id"]}))["status"],
            expected
        );
        assert_eq!(
            fixture
                .requests()
                .iter()
                .filter(|r| r.body["method"] == "tools/call")
                .count(),
            usize::from(decision != "deny")
        );
        drop(peer);
        fs::remove_dir_all(root).unwrap();
    }
}
