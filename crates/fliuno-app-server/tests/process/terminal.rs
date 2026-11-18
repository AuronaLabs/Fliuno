//! Real native PTYs, independent supervisor processes, stdio RPC, approval
//! and restart. No network credentials or commercial model are required.
use super::*;
use base64::Engine;
use fliuno_terminal::{
    Service,
    protocol::{Operation, Status},
};

#[path = "terminal/archive.rs"]
mod archive;
#[path = "terminal/subscriptions.rs"]
mod subscriptions;

struct Directory {
    root: PathBuf,
    service: Option<Service>,
}
impl Directory {
    fn new(lifetime_ms: u64) -> Self {
        let root = std::env::temp_dir().join(format!(
            "fliuno-terminal-rpc-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&root).unwrap();
        fs::write(root.join("terminal.json"),serde_json::to_vec(&json!({"stateDirectory":root.join("terminals"),"presets":[{"id":"probe","command":env!("CARGO_BIN_EXE_fliuno-app-exec-fixture"),"args":["terminal-fixture"],"isolation":"trusted_host","lifetimeMs":lifetime_ms,"spoolBytes":65536}]})).unwrap()).unwrap();
        // Independent client facade for emergency cleanup on an assertion panic.
        let mut kernel =
            fliuno_core::Kernel::in_memory(Box::new(fliuno_core::DemoProvider)).unwrap();
        let service = fliuno_terminal::register_config(
            &mut kernel,
            serde_json::from_slice(&fs::read(root.join("terminal.json")).unwrap()).unwrap(),
            PathBuf::from(env!("CARGO_BIN_EXE_fliuno-app-server")),
        )
        .unwrap();
        Self {
            root,
            service: Some(service),
        }
    }
    fn peer(&self, base: &str) -> Peer {
        Peer::start_with_all_configuration(
            &self.root.join("kernel.db"),
            Some(base),
            None,
            None,
            Some(&self.root.join("terminal.json")),
        )
    }
}
impl Drop for Directory {
    fn drop(&mut self) {
        if let Some(service) = self.service.take() {
            let rt = tokio::runtime::Runtime::new().unwrap();
            if let Ok(entries) = fs::read_dir(self.root.join("terminals")) {
                for entry in entries.flatten() {
                    let file = entry.path().join("output.db");
                    if !file.exists() {
                        continue;
                    }
                    // Test-only inspection resolves the session from durable state,
                    // never by guessing a process PID or deleting a live directory.
                    if let Ok(db) = rusqlite::Connection::open_with_flags(
                        file,
                        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
                    ) && let Ok(s) =
                        db.query_row("SELECT data FROM snapshot", [], |r| r.get::<_, String>(0))
                        && let Ok(s) = serde_json::from_str::<fliuno_protocol::TerminalSnapshot>(&s)
                    {
                        let _ = rt.block_on(service.call(
                            &s.session_id,
                            &s.terminal_id,
                            Operation::Release {
                                operation_id: "f".repeat(64),
                            },
                        ));
                    }
                }
            }
        }
        // Worker shutdown is bounded, but the OS may still be closing SQLite.
        for _ in 0..100 {
            if fs::remove_dir_all(&self.root).is_ok() {
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }
    }
}
fn model() -> http_wire::Fixture {
    http_wire::Fixture::start(|request| {
        let input = request.body["input"].as_array().unwrap();
        if let Some(result) = input.iter().find(|m| m["type"] == "function_call_output") {
            return http_wire::Response::json(
                200,
                json!({"status":"completed","output":[{"type":"message","content":[{"type":"output_text","text":result["output"]}]}]}),
            );
        }
        let instruction: Value =
            serde_json::from_str(input.last().unwrap()["content"].as_str().unwrap()).unwrap();
        let prefix = if instruction["operation"] == "start" {
            "terminal_probe_".to_string()
        } else {
            format!("terminal_{}_", instruction["operation"].as_str().unwrap())
        };
        let name = request.body["tools"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["name"].as_str().unwrap().starts_with(&prefix))
            .unwrap()["name"]
            .clone();
        http_wire::Response::json(
            200,
            json!({"status":"completed","output":[{"type":"function_call","call_id":"terminal_operation","name":name,"arguments":instruction["arguments"].to_string()}]}),
        )
    })
}
fn session(peer: &mut Peer, dir: &Directory) -> Value {
    let w = peer.call("workspace/open", json!({"root":dir.root}));
    peer.call("session/create", json!({"workspaceId":w["id"]}))
}
fn propose(peer: &mut Peer, session: &Value, op: &str, args: Value, key: &str) -> Value {
    let task=peer.call("task/start",json!({"sessionId":session["id"],"input":json!({"operation":op,"arguments":args}).to_string(),"clientRequestId":key}));
    peer.wait_for_status(task["id"].as_str().unwrap(), "waiting_approval")
}
fn allow(peer: &mut Peer, task: &Value) -> Value {
    peer.call("approval/respond",json!({"taskId":task["id"],"approvalId":task["pendingApproval"]["id"],"decision":"allow_once"}));
    let done = peer.wait_for_status(task["id"].as_str().unwrap(), "completed");
    serde_json::from_str(done["output"].as_str().unwrap()).unwrap()
}
fn execute(peer: &mut Peer, session: &Value, op: &str, args: Value, key: &str) -> Value {
    let t = propose(peer, session, op, args, key);
    allow(peer, &t)
}
fn output(peer: &mut Peer, session: &Value, id: &str) -> Value {
    peer.call(
        "terminal/output",
        json!({"sessionId":session["id"],"terminalId":id,"after":0,"limit":32768}),
    )
}
fn enter(text: &str) -> String {
    format!("{text}{}", if cfg!(windows) { "\r" } else { "\n" })
}
fn text(output: &Value) -> String {
    String::from_utf8_lossy(
        &base64::engine::general_purpose::STANDARD
            .decode(output["dataBase64"].as_str().unwrap())
            .unwrap(),
    )
    .into_owned()
}
fn wait_text(peer: &mut Peer, session: &Value, id: &str, needle: &str) -> Value {
    for _ in 0..300 {
        let out = output(peer, session, id);
        if text(&out).contains(needle) {
            return out;
        }
        thread::sleep(Duration::from_millis(10));
    }
    panic!(
        "terminal output missing {needle}: {}",
        output(peer, session, id)
    );
}

#[test]
fn approved_real_pty_survives_server_restart_and_each_input_is_independently_approved() {
    let dir = Directory::new(30000);
    let fixture = model();
    let base = fixture.url("/v1/");
    let mut peer = dir.peer(&base);
    let session = session(&mut peer, &dir);
    let task = propose(
        &mut peer,
        &session,
        "start",
        json!({"rows":25,"cols":90}),
        "start",
    );
    assert_eq!(
        peer.call("terminal/list", json!({"sessionId":session["id"]})),
        json!([])
    );
    let started = allow(&mut peer, &task);
    let id = started["terminalId"].as_str().unwrap();
    let ready = wait_text(&mut peer, &session, id, "TERMINAL_READY");
    let state_line = text(&ready);
    assert!(state_line.contains("\"stdinTty\":true"), "{state_line}");
    assert!(state_line.contains("\"stdoutTty\":true"));
    assert!(state_line.contains("\"secret\":false"));
    drop(peer);
    let mut peer = dir.peer(&base);
    let status = peer.call(
        "terminal/get",
        json!({"sessionId":session["id"],"terminalId":id}),
    );
    assert_eq!(status["status"], "running");
    assert_eq!(status["rows"], 25);
    let workspaces = peer.call("workspace/list", json!({}));
    let other = peer.call(
        "session/create",
        json!({"workspaceId":workspaces["items"][0]["id"]}),
    );
    assert!(
        peer.request(
            "terminal/get",
            json!({"sessionId":other["id"],"terminalId":id})
        )
        .get("error")
        .is_some()
    );
    let denied = propose(
        &mut peer,
        &session,
        "write",
        json!({"terminalId":id,"data":"DENIED_INPUT\r\n"}),
        "denied",
    );
    peer.call("approval/respond",json!({"taskId":denied["id"],"approvalId":denied["pendingApproval"]["id"],"decision":"deny"}));
    assert!(!text(&output(&mut peer, &session, id)).contains("DENIED_INPUT"));
    execute(
        &mut peer,
        &session,
        "resize",
        json!({"terminalId":id,"rows":40,"cols":120}),
        "resize",
    );
    let status = peer.call(
        "terminal/get",
        json!({"sessionId":session["id"],"terminalId":id}),
    );
    assert_eq!(status["rows"], 40);
    assert_eq!(status["cols"], 120);
    let args = json!({"terminalId":id,"data":enter("ping 中文")});
    let before = propose(&mut peer, &session, "write", args.clone(), "input");
    let written = allow(&mut peer, &before);
    assert_eq!(written["writtenBytes"], "ping 中文\n".len());
    wait_text(&mut peer, &session, id, "TERMINAL_REPLY ping 中文");
    let retried=peer.call("task/start",json!({"sessionId":session["id"],"input":json!({"operation":"write","arguments":args}).to_string(),"clientRequestId":"input"}));
    assert_eq!(retried["id"], before["id"]);
    let status = peer.call(
        "terminal/get",
        json!({"sessionId":session["id"],"terminalId":id}),
    );
    assert_eq!(status["inputBytes"], "ping 中文\n".len());
    execute(
        &mut peer,
        &session,
        "release",
        json!({"terminalId":id}),
        "release",
    );
    thread::sleep(Duration::from_millis(900));
    assert_eq!(
        peer.call(
            "terminal/get",
            json!({"sessionId":session["id"],"terminalId":id})
        )["status"],
        "released"
    );
}

#[cfg(target_os = "linux")]
#[path = "terminal/linux.rs"]
mod linux;

#[test]
fn pty_output_flood_spools_bounded_tails_and_exit_preserves_final_output() {
    let dir = Directory::new(30000);
    let fixture = model();
    let base = fixture.url("/v1/");
    let mut peer = dir.peer(&base);
    let session = session(&mut peer, &dir);
    let start = execute(&mut peer, &session, "start", json!({}), "start");
    let id = start["terminalId"].as_str().unwrap();
    wait_text(&mut peer, &session, id, "TERMINAL_READY");
    execute(
        &mut peer,
        &session,
        "write",
        json!({"terminalId":id,"data":enter("flood")}),
        "flood",
    );
    let mut observed = false;
    for _ in 0..300 {
        let status = peer.call(
            "terminal/get",
            json!({"sessionId":session["id"],"terminalId":id}),
        );
        if status["endOffset"].as_u64().unwrap() > 256 * 1024 {
            observed = true;
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    assert!(observed);
    let out = output(&mut peer, &session, id);
    assert_eq!(out["gap"], true);
    assert!(out["snapshot"]["startOffset"].as_u64().unwrap() > 0);
    assert!(
        out["snapshot"]["endOffset"].as_u64().unwrap()
            - out["snapshot"]["startOffset"].as_u64().unwrap()
            <= 65536
    );
    // Crossing the byte threshold only proves that some flood data was
    // spooled. The final marker may still be in the native reader/channel.
    // Follow the byte cursor until the marker, including split marker chunks.
    let mut cursor = out["next"].as_u64().unwrap();
    let mut marker = String::new();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let last = peer.call(
            "terminal/output",
            json!({"sessionId":session["id"],"terminalId":id,"after":cursor,"limit":32768}),
        );
        cursor = last["next"].as_u64().unwrap();
        marker.push_str(&text(&last));
        if marker.contains("FLOOD_END") {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "flood marker was not drained: {last}"
        );
        marker = marker
            .chars()
            .rev()
            .take(32)
            .collect::<String>()
            .chars()
            .rev()
            .collect();
        thread::sleep(Duration::from_millis(10));
    }
    execute(
        &mut peer,
        &session,
        "write",
        json!({"terminalId":id,"data":enter("exit")}),
        "exit",
    );
    for _ in 0..300 {
        let s = peer.call(
            "terminal/get",
            json!({"sessionId":session["id"],"terminalId":id}),
        );
        if s["status"] == "exited" && s["outputClosed"] == true {
            assert_eq!(s["exitCode"], 0);
            let final_output = peer.call(
                "terminal/output",
                json!({"sessionId":session["id"],"terminalId":id,"after":cursor,"limit":32768}),
            );
            assert!(
                text(&final_output).contains("TERMINAL_EXIT"),
                "exit bytes were not preserved: {final_output}"
            );
            return;
        }
        thread::sleep(Duration::from_millis(10));
    }
    panic!("PTY did not drain after child exit");
}

#[test]
fn worker_expiry_stops_process_without_server_and_keeps_offline_output() {
    let dir = Directory::new(3000);
    let fixture = model();
    let base = fixture.url("/v1/");
    let mut peer = dir.peer(&base);
    let session = session(&mut peer, &dir);
    let start = execute(&mut peer, &session, "start", json!({}), "start");
    let id = start["terminalId"].as_str().unwrap();
    wait_text(&mut peer, &session, id, "TERMINAL_READY");
    drop(peer);
    let service = dir.service.as_ref().unwrap();
    let rt = tokio::runtime::Runtime::new().unwrap();
    for _ in 0..500 {
        let s = rt
            .block_on(service.status(session["id"].as_str().unwrap(), id))
            .unwrap();
        if s.status == Status::Expired {
            let out = rt
                .block_on(service.output(&s.session_id, id, 0, 32768))
                .unwrap();
            assert!(!out.data_base64.is_empty());
            return;
        }
        thread::sleep(Duration::from_millis(10));
    }
    panic!("terminal lifetime did not expire");
}

#[test]
fn terminal_worker_caches_input_identity_and_rejects_conflicting_replay() {
    let dir = Directory::new(30000);
    let fixture = model();
    let base = fixture.url("/v1/");
    let mut peer = dir.peer(&base);
    let session = session(&mut peer, &dir);
    let start = execute(&mut peer, &session, "start", json!({}), "start");
    let id = start["terminalId"].as_str().unwrap();
    wait_text(&mut peer, &session, id, "TERMINAL_READY");
    let service = dir.service.as_ref().unwrap();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let operation = Operation::Write {
        operation_id: "a".repeat(64),
        data: enter("once-only"),
    };
    let first = rt
        .block_on(service.call(session["id"].as_str().unwrap(), id, operation.clone()))
        .unwrap();
    wait_text(&mut peer, &session, id, "TERMINAL_REPLY once-only");
    let second = rt
        .block_on(service.call(session["id"].as_str().unwrap(), id, operation))
        .unwrap();
    assert_eq!(first, second);
    assert!(
        rt.block_on(service.call(
            session["id"].as_str().unwrap(),
            id,
            Operation::Write {
                operation_id: "a".repeat(64),
                data: enter("changed")
            }
        ))
        .unwrap_err()
        .contains("different")
    );
    let state = rt
        .block_on(service.status(session["id"].as_str().unwrap(), id))
        .unwrap();
    assert_eq!(state.input_bytes, enter("once-only").len() as u64);
    let empty = Operation::Write {
        operation_id: "b".repeat(64),
        data: String::new(),
    };
    let e1 = rt
        .block_on(service.call(session["id"].as_str().unwrap(), id, empty.clone()))
        .unwrap_err();
    let e2 = rt
        .block_on(service.call(session["id"].as_str().unwrap(), id, empty))
        .unwrap_err();
    assert_eq!(e1, e2);
    assert!(!e2.contains("unknown"));
}

#[test]
fn failed_native_pty_start_keeps_a_session_owned_record_diagnostic_and_audit_identity() {
    let dir = Directory::new(30000);
    let bad_program = dir.root.join("invalid-program.exe");
    fs::write(&bad_program, b"not a native executable").unwrap();
    let path = dir.root.join("terminal.json");
    let mut config: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    config["presets"][0]["command"] = json!(bad_program);
    fs::write(&path, serde_json::to_vec(&config).unwrap()).unwrap();
    let fixture = model();
    let base = fixture.url("/v1/");
    let mut peer = dir.peer(&base);
    let session = session(&mut peer, &dir);
    let pending = propose(&mut peer, &session, "start", json!({}), "failed-start");
    peer.call("approval/respond", json!({"taskId":pending["id"],"approvalId":pending["pendingApproval"]["id"],"decision":"allow_once"}));
    let interrupted = peer.wait_for_status(pending["id"].as_str().unwrap(), "interrupted");
    assert!(interrupted["inFlightTool"].is_object());
    let events = peer.call("events/list", json!({"taskId":pending["id"]}));
    let evidence = &events["events"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["data"]["type"] == "tool_outcome_unknown")
        .unwrap()["data"]["output"];
    let id = evidence["terminalId"].as_str().unwrap();
    let output = output(&mut peer, &session, id);
    assert_eq!(output["snapshot"]["status"], "outcome_unknown");
    assert_eq!(output["snapshot"]["supervisorConnected"], false);
    assert_eq!(
        output["snapshot"]["startupError"],
        "cannot launch terminal preset"
    );
    assert_eq!(output["dataBase64"], "");
    drop(peer);
    let mut peer = dir.peer(&base);
    let listed = peer.call("terminal/list", json!({"sessionId":session["id"]}));
    assert_eq!(listed.as_array().unwrap().len(), 1);
    assert_eq!(listed[0]["terminalId"], id);
    assert_eq!(listed[0]["startupError"], "cannot launch terminal preset");
    assert_eq!(
        peer.call("events/list", json!({"taskId":pending["id"]})),
        events
    );
    assert!(
        peer.request(
            "task/resume",
            json!({"taskId":pending["id"],"retryUncertainModelStep":true})
        )
        .get("error")
        .is_some()
    );
}

#[cfg(windows)]
fn wait_stopped(pid: u32) {
    for _ in 0..300 {
        if !fliuno_sandbox::process_is_running(pid).unwrap() {
            return;
        }
        thread::sleep(Duration::from_millis(10));
    }
    panic!("owned process {pid} remained alive");
}
#[cfg(windows)]
fn descendant(peer: &mut Peer, session: &Value, id: &str) -> u32 {
    let out = wait_text(peer, session, id, "DESCENDANT ");
    let text = text(&out);
    text.split("DESCENDANT ")
        .nth(1)
        .unwrap()
        .chars()
        .take_while(char::is_ascii_digit)
        .collect::<String>()
        .parse()
        .unwrap()
}

#[cfg(windows)]
#[test]
fn windows_terminal_job_reaps_descendants_on_kill_and_natural_root_exit() {
    for natural in [false, true] {
        let dir = Directory::new(30000);
        let fixture = model();
        let base = fixture.url("/v1/");
        let mut peer = dir.peer(&base);
        let session = session(&mut peer, &dir);
        let start = execute(&mut peer, &session, "start", json!({}), "start");
        let id = start["terminalId"].as_str().unwrap();
        wait_text(&mut peer, &session, id, "TERMINAL_READY");
        execute(
            &mut peer,
            &session,
            "write",
            json!({"terminalId":id,"data":enter(if natural{"spawn-exit"}else{"spawn"})}),
            "spawn",
        );
        let pid = descendant(&mut peer, &session, id);
        if !natural {
            assert!(fliuno_sandbox::process_is_running(pid).unwrap());
            execute(
                &mut peer,
                &session,
                "kill",
                json!({"terminalId":id}),
                "kill",
            );
        }
        wait_stopped(pid);
        let state = peer.call(
            "terminal/get",
            json!({"sessionId":session["id"],"terminalId":id}),
        );
        assert_eq!(state["status"], if natural { "exited" } else { "killed" });
    }
}

#[cfg(windows)]
#[test]
fn windows_supervisor_hard_crash_reaps_descendants_and_never_relaunches_terminal() {
    let dir = Directory::new(30000);
    let fixture = model();
    let base = fixture.url("/v1/");
    let mut peer = dir.peer(&base);
    let session = session(&mut peer, &dir);
    let start = execute(&mut peer, &session, "start", json!({}), "start");
    let id = start["terminalId"].as_str().unwrap();
    let ready = wait_text(&mut peer, &session, id, "TERMINAL_READY");
    let subscription = subscriptions::subscribe(
        &mut peer,
        &session,
        id,
        ready["next"].as_u64().unwrap(),
        32768,
    );
    execute(
        &mut peer,
        &session,
        "write",
        json!({"terminalId":id,"data":enter("spawn")}),
        "spawn",
    );
    let child = descendant(&mut peer, &session, id);
    let descriptor: fliuno_terminal::protocol::Descriptor = serde_json::from_slice(
        &fs::read(
            dir.service
                .as_ref()
                .unwrap()
                .directory(id)
                .unwrap()
                .join("endpoint.json"),
        )
        .unwrap(),
    )
    .unwrap();
    assert!(fliuno_sandbox::process_is_running(descriptor.supervisor_pid).unwrap());
    let taskkill =
        PathBuf::from(std::env::var_os("SystemRoot").unwrap()).join("System32/taskkill.exe");
    use std::os::windows::process::CommandExt;
    assert!(
        Command::new(taskkill)
            .args(["/F", "/PID", &descriptor.supervisor_pid.to_string()])
            .creation_flags(0x08000000)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap()
            .success()
    );
    wait_stopped(descriptor.supervisor_pid);
    wait_stopped(child);
    let end = subscriptions::notification(
        &mut peer,
        &subscription,
        "notifications/terminalSubscriptionEnded",
    );
    assert_eq!(end["reason"], "supervisor_unavailable");
    let last = peer
        .notifications
        .iter()
        .rev()
        .find(|n| {
            n["method"] == "notifications/terminalOutput"
                && n["params"]["subscriptionId"] == subscription
        })
        .expect("final offline observation");
    assert_eq!(
        last["params"]["output"]["snapshot"]["supervisorConnected"],
        false
    );
    assert_eq!(
        last["params"]["output"]["snapshot"]["status"],
        "outcome_unknown"
    );
    let replay = subscriptions::subscribe(
        &mut peer,
        &session,
        id,
        end["after"].as_u64().unwrap(),
        32768,
    );
    let offline = subscriptions::notification(&mut peer, &replay, "notifications/terminalOutput");
    assert_eq!(offline["output"]["snapshot"]["supervisorConnected"], false);
    assert_eq!(offline["output"]["next"], end["after"]);
    assert_eq!(
        subscriptions::notification(
            &mut peer,
            &replay,
            "notifications/terminalSubscriptionEnded"
        )["reason"],
        "supervisor_unavailable"
    );
    for _ in 0..3 {
        let s = peer.call(
            "terminal/get",
            json!({"sessionId":session["id"],"terminalId":id}),
        );
        assert_eq!(s["status"], "outcome_unknown");
        assert_eq!(s["supervisorConnected"], false);
        assert!(text(&output(&mut peer, &session, id)).contains("TERMINAL_READY"));
    }
    let listed = peer.call("terminal/list", json!({"sessionId":session["id"]}));
    assert_eq!(listed.as_array().unwrap().len(), 1);
}
