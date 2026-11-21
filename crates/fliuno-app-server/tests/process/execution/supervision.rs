use super::*;

fn fixture() -> Fixture {
    Fixture::start(|request| {
        let inputs = request.body["input"].as_array().unwrap();
        if let Some(result) = inputs
            .iter()
            .find(|item| item["type"] == "function_call_output")
        {
            let report: Value = serde_json::from_str(result["output"].as_str().unwrap()).unwrap();
            assert_eq!(report["termination"], "exited");
            return Response::json(
                200,
                json!({"status":"completed","output":[{
                    "type":"message","content":[{"type":"output_text","text":"Observed supervised result"}]
                }]}),
            );
        }
        let name = request.body["tools"]
            .as_array()
            .unwrap()
            .iter()
            .find_map(|tool| {
                tool["name"]
                    .as_str()
                    .filter(|name| name.starts_with("process_probe_"))
            })
            .unwrap();
        Response::json(
            200,
            json!({"status":"completed","output":[{
                "type":"function_call","call_id":"run-handoff","name":name,
                "arguments":json!({"args":["run-handoff"]}).to_string()
            }]}),
        )
    })
}

fn wait_until(mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(8);
    while !ready() {
        assert!(
            Instant::now() < deadline,
            "supervised process did not reach checkpoint"
        );
        thread::sleep(Duration::from_millis(10));
    }
}

fn start_run(peer: &mut Peer, directory: &Directory) -> (String, String, u32) {
    start_run_with_deadline(peer, directory, None)
}

fn start_run_with_deadline(
    peer: &mut Peer,
    directory: &Directory,
    timeout: Option<u64>,
) -> (String, String, u32) {
    let session = session(peer, directory);
    let mut request = json!({"sessionId":session["id"],"clientRequestId":"supervised-run","nodes":[{
        "nodeId":"command","input":"run the fixture"
    }]});
    if let Some(timeout) = timeout {
        request["timeoutMs"] = json!(timeout);
    }
    let run = peer.call("run/start", request);
    let run_id = run["id"].as_str().unwrap().to_owned();
    let task_id = run["nodes"][0]["taskId"].as_str().unwrap().to_owned();
    let waiting = peer.wait_for_status(&task_id, "waiting_approval");
    assert!(!directory.0.join("handoff.starts").exists());
    allow(peer, &waiting);
    wait_until(|| directory.0.join("handoff.starts").exists());
    let db = rusqlite::Connection::open(directory.0.join("kernel.db")).unwrap();
    let supervisor_pid = db
        .query_row(
            "SELECT worker_pid FROM supervised_tools WHERE task_id=?1 AND phase=1",
            [&task_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_ne!(supervisor_pid, peer.child.id());
    (run_id, task_id, supervisor_pid)
}

fn event_types(peer: &mut Peer, task_id: &str) -> Vec<String> {
    peer.call("events/list", json!({"taskId":task_id}))["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|event| event["data"]["type"].as_str().unwrap().into())
        .collect()
}

fn assert_one_execution(directory: &Directory, events: &[String]) {
    assert_eq!(
        fs::read_to_string(directory.0.join("handoff.starts")).unwrap(),
        "started\n"
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| *event == "tool_started")
            .count(),
        1
    );
}

#[test]
fn supervisor_survives_coordinator_kill_and_reattaches_to_the_original_run() {
    let directory = Directory::new();
    let fixture = fixture();
    let base = fixture.url("/v1/");
    let mut peer = directory.peer(&base);
    let (run_id, task_id, pid) = start_run(&mut peer, &directory);
    let cursor = peer.call("run/events", json!({"runId":run_id}))["nextAfter"].clone();
    drop(peer);
    let mut peer = directory.peer(&base);
    assert_eq!(
        peer.call("task/get", json!({"taskId":task_id}))["status"],
        "running"
    );
    let db = rusqlite::Connection::open(directory.0.join("kernel.db")).unwrap();
    let saved_pid: u32 = db
        .query_row(
            "SELECT worker_pid FROM supervised_tools WHERE task_id=?1",
            [&task_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(pid, saved_pid);
    fs::write(directory.0.join("handoff.release"), "release").unwrap();
    let done = peer.wait_for_status(&task_id, "completed");
    assert_eq!(done["output"], "Observed supervised result");
    assert_one_execution(&directory, &event_types(&mut peer, &task_id));
    let run = peer.call("run/get", json!({"runId":run_id}));
    assert_eq!(run["status"], "completed");
    assert_eq!(run["nodes"][0]["taskId"], task_id);
    let resumed = peer.call("run/events", json!({"runId":run_id,"after":cursor}));
    assert!(!resumed["events"].as_array().unwrap().is_empty());
    assert!(
        resumed["events"]
            .as_array()
            .unwrap()
            .iter()
            .all(|e| e["sequence"].as_u64() > cursor.as_u64())
    );
}

#[test]
fn killed_supervisor_leaves_unknown_result_without_replaying() {
    let directory = Directory::new();
    let fixture = fixture();
    let base = fixture.url("/v1/");
    let mut peer = directory.peer(&base);
    let (run_id, task_id, pid) = start_run(&mut peer, &directory);
    #[cfg(windows)]
    let status = Command::new("taskkill.exe")
        .args(["/PID", &pid.to_string(), "/F"])
        .output()
        .unwrap();
    #[cfg(target_os = "linux")]
    let status = Command::new("kill")
        .args(["-KILL", &pid.to_string()])
        .output()
        .unwrap();
    assert!(
        status.status.success(),
        "could not stop supervisor: {status:?}"
    );
    drop(peer);
    let mut peer = directory.peer(&base);
    peer.wait_for_status(&task_id, "interrupted");
    assert_eq!(
        peer.call("run/get", json!({"runId":run_id}))["status"],
        "interrupted"
    );
    let events = event_types(&mut peer, &task_id);
    assert!(events.iter().any(|event| event == "tool_outcome_unknown"));
    assert!(!events.iter().any(|event| event == "tool_completed"));
    assert_one_execution(&directory, &events);
    fs::write(directory.0.join("handoff.release"), "release").unwrap();
    // Without a delegated cgroup, Linux cannot prove that an already launched
    // process tree stopped when the supervisor died; it must stay uncertain.
    #[cfg(windows)]
    assert!(!directory.0.join("handoff.completed").exists());
}

#[test]
fn changed_tool_registration_cannot_accept_the_old_supervisor_result() {
    let directory = Directory::new();
    let fixture = fixture();
    let base = fixture.url("/v1/");
    let mut peer = directory.peer(&base);
    let (run_id, task_id, _) = start_run(&mut peer, &directory);
    drop(peer);
    let path = directory.0.join("execution.json");
    let mut config: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    config["programs"][0]["timeoutMs"] = json!(4000);
    fs::write(&path, serde_json::to_vec(&config).unwrap()).unwrap();
    let mut peer = directory.peer(&base);
    let interrupted = peer.wait_for_status(&task_id, "interrupted");
    assert!(
        interrupted["output"]
            .as_str()
            .unwrap()
            .contains("authority")
    );
    assert_eq!(
        peer.call("run/get", json!({"runId":run_id}))["status"],
        "interrupted"
    );
    let events = event_types(&mut peer, &task_id);
    assert!(events.iter().any(|event| event == "tool_outcome_unknown"));
    assert!(!events.iter().any(|event| event == "tool_completed"));
    assert_one_execution(&directory, &events);
}

#[test]
fn supervisor_checks_run_deadline_while_coordinator_is_offline() {
    let directory = Directory::new();
    let fixture = fixture();
    let base = fixture.url("/v1/");
    let mut peer = directory.peer(&base);
    let (run_id, task_id, pid) = start_run_with_deadline(&mut peer, &directory, Some(2400));
    drop(peer);
    wait_until(|| {
        #[cfg(windows)]
        {
            !fliuno_sandbox::process_is_running(pid).unwrap()
        }
        #[cfg(target_os = "linux")]
        {
            fs::read_to_string(format!("/proc/{pid}/stat")).map_or(true, |stat| {
                stat.rsplit_once(") ")
                    .is_some_and(|(_, fields)| fields.starts_with('Z'))
            })
        }
    });
    assert!(!directory.0.join("handoff.completed").exists());
    let mut peer = directory.peer(&base);
    assert_eq!(
        peer.call("run/get", json!({"runId":run_id}))["status"],
        "timed_out"
    );
    assert_eq!(
        peer.call("task/get", json!({"taskId":task_id}))["status"],
        "cancelled"
    );
    assert_one_execution(&directory, &event_types(&mut peer, &task_id));
}

#[test]
fn run_cancellation_revokes_supervisor_ownership_and_wins_over_a_late_result() {
    let directory = Directory::new();
    let fixture = fixture();
    let base = fixture.url("/v1/");
    let mut peer = directory.peer(&base);
    let (run_id, task_id, _) = start_run(&mut peer, &directory);
    assert_eq!(
        peer.call("run/cancel", json!({"runId":run_id}))["status"],
        "cancelled"
    );
    fs::write(directory.0.join("handoff.release"), "release").unwrap();
    drop(peer);
    let mut peer = directory.peer(&base);
    assert_eq!(
        peer.call("task/get", json!({"taskId":task_id}))["status"],
        "cancelled"
    );
    assert_eq!(
        peer.call("run/get", json!({"runId":run_id}))["status"],
        "cancelled"
    );
    let events = event_types(&mut peer, &task_id);
    assert!(!events.iter().any(|event| event == "tool_completed"));
    assert_one_execution(&directory, &events);
}

#[test]
fn committed_child_reconciles_when_coordinator_could_not_commit_parent() {
    let directory = Directory::new();
    let fixture = fixture();
    let base = fixture.url("/v1/");
    let mut peer = directory.peer(&base);
    let (run_id, task_id, _) = start_run(&mut peer, &directory);
    let db = rusqlite::Connection::open(directory.0.join("kernel.db")).unwrap();
    db.execute_batch(
        "CREATE TRIGGER block_final_run BEFORE UPDATE ON runs
        WHEN NEW.status='completed' BEGIN SELECT RAISE(ABORT,'fixture block'); END;",
    )
    .unwrap();
    fs::write(directory.0.join("handoff.release"), "release").unwrap();
    peer.wait_for_status(&task_id, "completed");
    let status: String = db
        .query_row("SELECT status FROM runs WHERE id=?1", [&run_id], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(status, "running");
    drop(peer);
    db.execute_batch("DROP TRIGGER block_final_run").unwrap();
    let mut peer = directory.peer(&base);
    assert_eq!(
        peer.call("run/get", json!({"runId":run_id}))["status"],
        "completed"
    );
    assert_one_execution(&directory, &event_types(&mut peer, &task_id));
}

#[test]
fn supervisor_trace_links_the_process_measurements_to_its_parent_tool() {
    let directory = Directory::new();
    let fixture = fixture();
    let trace_file = directory.0.join("fliuno-trace.jsonl");
    let mut command = Command::new(env!("CARGO_BIN_EXE_fliuno-app-server"));
    command
        .arg("--db")
        .arg(directory.0.join("kernel.db"))
        .arg("--exec-config")
        .arg(directory.0.join("execution.json"))
        .arg("--trace-log")
        .arg(&trace_file)
        .env("FLIUNO_PROVIDER", "openai")
        .env("FLIUNO_MODEL", "fixture-model")
        .env("OPENAI_API_KEY", "TRACE_PRIVATE_TOKEN_TEST")
        .env("OPENAI_BASE_URL", fixture.url("/v1/"))
        .env("OTEL_EXPORTER_OTLP_ENDPOINT", fixture.url("/v1/traces"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    let mut peer = Peer::from_child(command.spawn().unwrap());
    peer.call(
        "initialize",
        json!({"protocolVersion":"0.1","clientName":"supervisor-trace-test"}),
    );
    let (run_id, task_id, _) = start_run(&mut peer, &directory);
    fs::write(directory.0.join("handoff.release"), "release").unwrap();
    peer.wait_for_status(&task_id, "completed");
    assert_eq!(
        peer.call("run/get", json!({"runId":run_id}))["status"],
        "completed"
    );

    let spans = |path: &std::path::Path| -> Vec<Value> {
        fs::read_to_string(path)
            .unwrap_or_default()
            .lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect()
    };
    let process_spans = || {
        fs::read_dir(&directory.0)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with("fliuno-trace.jsonl.worker-")
            })
            .flat_map(|entry| spans(&entry.path()))
            .collect::<Vec<_>>()
    };
    wait_until(|| {
        !process_spans().is_empty()
            && spans(&trace_file)
                .iter()
                .any(|span| span["operation"] == "tool")
    });
    let recorded_processes = process_spans();
    let process = recorded_processes
        .iter()
        .find(|span| span["operation"] == "process")
        .unwrap();
    assert_eq!(process["outcome"], "completed");
    let host = spans(&trace_file);
    let tool = host
        .iter()
        .find(|span| span["spanId"] == process["parentSpanId"])
        .unwrap();
    assert_eq!(tool["operation"], "tool");
    assert_eq!(tool["traceId"], process["traceId"]);
    assert_eq!(tool["runRef"], process["runRef"]);
    assert_eq!(tool["taskRef"], process["taskRef"]);
    if cfg!(windows) {
        assert!(process["measurements"]["cpuMs"].is_u64());
        assert!(process["measurements"]["peakMemoryBytes"].as_u64().unwrap() > 0);
    }
    for private in ["run the fixture", "run-handoff", "TRACE_PRIVATE_TOKEN_TEST"] {
        assert!(
            !json!([recorded_processes, host])
                .to_string()
                .contains(private)
        );
    }
    assert!(!fixture.requests().iter().any(|r| r.path == "/v1/traces"));
}
