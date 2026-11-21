//! Executable RPC -> provider wire -> approval -> real process -> durable log.
use super::*;
use http_wire::{Fixture, Response};

#[cfg(any(windows, target_os = "linux"))]
#[path = "execution/supervision.rs"]
mod supervision;

struct Directory(PathBuf);
impl Directory {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "fliuno-exec-rpc-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&path).unwrap();
        fs::write(path.join("execution.json"),serde_json::to_vec(&json!({"programs":[{
            "id":"probe","command":env!("CARGO_BIN_EXE_fliuno-app-exec-fixture"),"isolation":"trusted_host","timeoutMs":5000,"outputBytes":4096
        }]})).unwrap()).unwrap();
        Self(path)
    }
    fn peer(&self, base: &str) -> Peer {
        Peer::start_with_configuration(
            &self.0.join("kernel.db"),
            Some(base),
            None,
            Some(&self.0.join("execution.json")),
        )
    }
}
impl Drop for Directory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn model() -> Fixture {
    Fixture::start(|request| {
        assert_eq!(request.path, "/v1/responses");
        let input = request.body["input"].as_array().unwrap();
        if let Some(result) = input
            .iter()
            .find(|item| item["type"] == "function_call_output")
        {
            let report: Value = serde_json::from_str(result["output"].as_str().unwrap()).unwrap();
            assert_eq!(report["termination"], "exited");
            return Response::json(
                200,
                json!({"status":"completed","output":[{"type":"message","content":[{"type":"output_text","text":"Observed program result"}]}]}),
            );
        }
        let text = input.last().unwrap()["content"].as_str().unwrap();
        let arguments = match text {
            "touch" => json!({"args":["touch","effect.txt"]}),
            "hang" => json!({"args":["hang-file","started.txt"]}),
            "timeout" => json!({"args":["hang"],"timeoutMs":1500}),
            _ => json!({"args":["echo","literal; value"],"stdin":"hello 中文"}),
        };
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
            json!({"status":"completed","output":[{"type":"function_call","call_id":"execute","name":name,"arguments":arguments.to_string()}]}),
        )
    })
}
fn session(peer: &mut Peer, directory: &Directory) -> Value {
    let workspace = peer.call("workspace/open", json!({"root":directory.0}));
    peer.call("session/create", json!({"workspaceId":workspace["id"]}))
}
fn start(peer: &mut Peer, session: &Value, input: &str, key: &str) -> Value {
    let task = peer.call(
        "task/start",
        json!({"sessionId":session["id"],"input":input,"clientRequestId":key}),
    );
    peer.wait_for_status(task["id"].as_str().unwrap(), "waiting_approval")
}
fn allow(peer: &mut Peer, task: &Value) {
    peer.call("approval/respond",json!({"taskId":task["id"],"approvalId":task["pendingApproval"]["id"],"decision":"allow_once"}));
}

#[test]
fn process_approval_survives_restart_and_retries_never_execute_twice() {
    let directory = Directory::new();
    let fixture = model();
    let base = fixture.url("/v1/");
    let mut peer = directory.peer(&base);
    let session = session(&mut peer, &directory);
    let tools = peer.call("tools/list", json!({}));
    assert!(
        tools
            .as_array()
            .unwrap()
            .iter()
            .any(|tool| tool["effect"] == "process_execution")
    );
    let task = start(&mut peer, &session, "touch", "once");
    assert!(!directory.0.join("effect.txt").exists());
    drop(peer);
    let mut peer = directory.peer(&base);
    let restored = peer.call("task/get", json!({"taskId":task["id"]}));
    assert_eq!(
        restored["pendingApproval"]["id"],
        task["pendingApproval"]["id"]
    );
    allow(&mut peer, &restored);
    peer.wait_for_status(task["id"].as_str().unwrap(), "completed");
    assert_eq!(
        fs::read_to_string(directory.0.join("effect.txt")).unwrap(),
        "executed"
    );
    fs::remove_file(directory.0.join("effect.txt")).unwrap();
    let retried = peer.call(
        "task/start",
        json!({"sessionId":session["id"],"input":"touch","clientRequestId":"once"}),
    );
    assert_eq!(retried["id"], task["id"]);
    assert!(!directory.0.join("effect.txt").exists());
    let events = peer.call("events/list", json!({"taskId":task["id"]}));
    assert_eq!(
        events["events"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|event| event["data"]["type"] == "tool_started")
            .count(),
        1
    );
    let denied = start(&mut peer, &session, "touch", "denied");
    peer.call("approval/respond",json!({"taskId":denied["id"],"approvalId":denied["pendingApproval"]["id"],"decision":"deny"}));
    assert!(!directory.0.join("effect.txt").exists());
}

#[test]
fn process_timeout_evidence_survives_restart_and_running_process_can_be_cancelled() {
    let directory = Directory::new();
    let fixture = model();
    let base = fixture.url("/v1/");
    let mut peer = directory.peer(&base);
    let session = session(&mut peer, &directory);
    let timeout = start(&mut peer, &session, "timeout", "timeout");
    allow(&mut peer, &timeout);
    let interrupted = peer.wait_for_status(timeout["id"].as_str().unwrap(), "interrupted");
    assert!(interrupted["inFlightTool"].is_object());
    let events = peer.call("events/list", json!({"taskId":timeout["id"]}));
    let report = &events["events"]
        .as_array()
        .unwrap()
        .iter()
        .find(|event| event["data"]["type"] == "tool_outcome_unknown")
        .unwrap()["data"]["output"];
    assert_eq!(report["termination"], "timed_out");
    assert_eq!(report["isolation"], "trusted_host");
    #[cfg(windows)]
    assert_eq!(report["containment"]["treeBackend"], "windows_job");
    #[cfg(target_os = "linux")]
    assert!(matches!(
        report["containment"]["treeBackend"].as_str(),
        Some("process_group" | "linux_cgroup_v2")
    ));
    #[cfg(any(windows, target_os = "linux"))]
    assert_eq!(
        report["containment"]["limits"],
        json!({"memoryMaxBytes":null,"pidsMax":null,"cpuQuotaMicros":null})
    );
    assert_eq!(report["evidence"]["stopCause"], "deadline");
    assert!(report["evidence"]["launchMs"].is_u64());
    assert_eq!(report["evidence"]["rootReaped"], true);
    // The deadline includes native process startup. A loaded Windows host can
    // time out before the fixture reaches main; an empty captured stream is
    // still valid evidence and must survive recovery without relaunching it.
    // Partial-output retention is tested separately in fliuno-exec.
    assert!(report["elapsedMs"].as_u64().unwrap() >= 1500);
    assert_eq!(
        report["stdout"]["retainedBytes"].as_u64().unwrap(),
        report["stdout"]["data"].as_str().unwrap().len() as u64,
        "{report}"
    );
    drop(peer);
    let mut peer = directory.peer(&base);
    assert_eq!(
        peer.call("events/list", json!({"taskId":timeout["id"]})),
        events
    );
    assert_eq!(
        peer.call("task/getResumeStatus", json!({"taskId":timeout["id"]}))["disposition"],
        "tool_outcome_unknown"
    );
    assert_eq!(
        peer.request(
            "task/resume",
            json!({"taskId":timeout["id"],"retryUncertainModelStep":true})
        )["error"]["code"],
        -32009
    );
    let task = start(&mut peer, &session, "hang", "cancel");
    allow(&mut peer, &task);
    let deadline = Instant::now() + Duration::from_secs(5);
    while !directory.0.join("started.txt").exists() {
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(
        peer.call("task/cancel", json!({"taskId":task["id"]}))["status"],
        "cancelled"
    );
    let next = start(&mut peer, &session, "echo", "next");
    allow(&mut peer, &next);
    let done = peer.wait_for_status(next["id"].as_str().unwrap(), "completed");
    let result = done["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|message| message["role"] == "tool_result")
        .unwrap();
    let probe: Value =
        serde_json::from_str(result["output"]["stdout"]["data"].as_str().unwrap()).unwrap();
    assert_eq!(probe["secret"], false);
    assert_eq!(probe["stdin"], "hello 中文");
    assert_eq!(
        peer.call("task/get", json!({"taskId":task["id"]}))["status"],
        "cancelled"
    );
}
