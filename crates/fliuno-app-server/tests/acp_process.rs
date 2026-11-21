//! End-to-end application RPC -> durable core -> ACP -> real isolated process.
#![cfg(all(any(windows, target_os = "linux"), feature = "test-fixtures"))]

use serde_json::{Value, json};
use std::{
    fs,
    io::{BufRead, BufReader, Write},
    path::PathBuf,
    process::{Child, ChildStdin, Command, Stdio},
    sync::mpsc::{self, Receiver},
    thread,
    time::{Duration, Instant},
};

#[path = "acp_process/authentication.rs"]
mod authentication;
#[path = "acp_process/authority.rs"]
mod authority;
#[path = "acp_process/catalog.rs"]
mod catalog;
#[path = "acp_process/context.rs"]
mod context;
#[path = "acp_process/persistence.rs"]
mod persistence;
#[path = "acp_process/terminal.rs"]
mod terminal;

struct Directory(PathBuf);
impl Directory {
    fn workspace_access(&self) {
        let path = self.0.join("agents.json");
        let mut config: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        config["agents"][0]["workspaceAccess"] = json!(true);
        fs::write(path, serde_json::to_vec(&config).unwrap()).unwrap();
    }
    fn timeouts(&self, prompt: u64, permission: u64) {
        let path = self.0.join("agents.json");
        let mut config: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        config["agents"][0]["promptTimeoutMs"] = json!(prompt);
        config["agents"][0]["permissionTimeoutMs"] = json!(permission);
        fs::write(path, serde_json::to_vec(&config).unwrap()).unwrap();
    }
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("fliuno-acp-rpc-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&root).unwrap();
        fs::write(
            root.join("agents.json"),
            serde_json::to_vec(&json!({"agents":[{
                "id":"fixture", "command":env!("CARGO_BIN_EXE_fliuno-app-acp-fixture"),
                // Exercise the production startup/request default. Windows
                // profile creation and executable staging are not a 5 s SLA;
                // protocol timeout/cancellation has separate controlled tests.
                "isolation":"native_scratch",
                "promptTimeoutMs":15000, "permissionTimeoutMs":5000
            }]}))
            .unwrap(),
        )
        .unwrap();
        Self(root)
    }
}
impl Drop for Directory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

struct Peer {
    child: Child,
    input: Option<ChildStdin>,
    output: Receiver<Value>,
    reader: Option<thread::JoinHandle<()>>,
    id: u64,
}
impl Peer {
    fn start(directory: &Directory) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_fliuno-app-server"))
            .args(["--db"])
            .arg(directory.0.join("kernel.db"))
            .arg("--acp-config")
            .arg(directory.0.join("agents.json"))
            .env("FLIUNO_PROVIDER", "demo")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let input = child.stdin.take();
        let stdout = child.stdout.take().unwrap();
        let (sender, output) = mpsc::channel();
        let reader = thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else {
                    break;
                };
                let value = serde_json::from_str(&line).expect("stdout must contain only JSON-RPC");
                if sender.send(value).is_err() {
                    break;
                }
            }
        });
        let mut peer = Self {
            child,
            input,
            output,
            reader: Some(reader),
            id: 0,
        };
        let init = peer.call(
            "initialize",
            json!({"protocolVersion":"0.1","clientName":"acp-test"}),
        );
        assert_eq!(init["capabilities"]["externalAgents"], true);
        assert_eq!(init["capabilities"]["registrationAuthority"], true);
        peer
    }
    fn request(&mut self, method: &str, params: Value) -> Value {
        self.id += 1;
        let input = self.input.as_mut().unwrap();
        writeln!(
            input,
            "{}",
            json!({"jsonrpc":"2.0","id":self.id,"method":method,"params":params})
        )
        .unwrap();
        input.flush().unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let response = self
                .output
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .unwrap_or_else(|e| panic!("RPC {method} timed out: {e}"));
            if response.get("method").is_some() {
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
    fn session(&mut self, directory: &Directory) -> String {
        let workspace = self.call("workspace/open", json!({"root":directory.0}));
        self.call("session/create", json!({"workspaceId":workspace["id"]}))["id"]
            .as_str()
            .unwrap()
            .into()
    }
    fn start_task(&mut self, session: &str, text: &str, key: &str) -> Value {
        self.call("task/start", json!({"sessionId":session,"input":text,"clientRequestId":key,"backend":{"type":"external_agent","agentId":"fixture"}}))
    }
    fn wait(&mut self, id: &Value, predicate: impl Fn(&Value) -> bool) -> Value {
        let deadline = Instant::now() + Duration::from_secs(35);
        loop {
            let task = self.call("task/get", json!({"taskId":id}));
            if predicate(&task) {
                return task;
            }
            assert!(
                Instant::now() < deadline,
                "task did not reach expected state: {task}"
            );
            thread::sleep(Duration::from_millis(10));
        }
    }
    fn stop(mut self) {
        drop(self.input.take());
        let deadline = Instant::now() + Duration::from_secs(8);
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                assert!(status.success());
                break;
            }
            assert!(
                Instant::now() < deadline,
                "app-server shutdown did not stop its agent"
            );
            thread::sleep(Duration::from_millis(10));
        }
    }
}
impl Drop for Peer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(reader) = self.reader.take() {
            reader.join().unwrap();
        }
    }
}

#[test]
fn durable_agent_approval_stream_and_completion_survive_reopen() {
    let directory = Directory::new();
    let mut peer = Peer::start(&directory);
    let agents = peer.call("agents/list", json!({}));
    assert_eq!(agents[0]["id"], "fixture");
    assert!(agents[0].get("command").is_none());
    assert_eq!(
        peer.call("agents/getCapabilities", json!({"agentId":"fixture"})),
        json!({"remoteTaskChecks":false,"remoteTaskWatch":false,
            "remoteTaskContinuation":false,"agentSessionContinuation":false})
    );
    let session = peer.session(&directory);
    let task = peer.start_task(&session, "permission", "once");
    let waiting = peer.wait(&task["id"], |t| t["status"] == "waiting_approval");
    let approval = waiting["agentRun"]["pendingApprovals"][0].clone();
    assert_eq!(waiting["modelSteps"], 0);
    assert_eq!(approval["allowOnceAvailable"], true);
    let inbox = peer.call("approval/listPending", json!({"sessionId":session}));
    assert_eq!(
        inbox["items"],
        json!([{ "id":approval["id"],"taskId":task["id"],
        "sessionId":session,"kind":"agent_permission","allowOnceAvailable":true,
        "expiresAtUnixMs":approval["expiresAtUnixMs"] }])
    );
    assert_eq!(
        peer.start_task(&session, "permission", "once")["id"],
        task["id"]
    );
    assert_eq!(
        peer.request(
            "task/start",
            json!({"sessionId":session,"input":"permission","clientRequestId":"once"})
        )["error"]["code"],
        -32009
    );
    peer.call(
        "approval/respond",
        json!({"taskId":task["id"],"approvalId":approval["id"],"decision":"allow_once"}),
    );
    let done = peer.wait(&task["id"], |t| t["status"] == "completed");
    assert!(
        done["output"]
            .as_str()
            .unwrap()
            .contains("\"optionId\":\"once\""),
        "{done}"
    );
    assert_eq!(done["agentRun"]["outcomeUnknown"], false);
    assert_eq!(done["agentRun"]["finished"], true);
    assert_eq!(done["agentRun"]["stopReason"], "end_turn");
    assert!(
        done["agentRun"]["pendingApprovals"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert!(
        peer.request(
            "approval/respond",
            json!({"taskId":task["id"],"approvalId":approval["id"],"decision":"allow_once"})
        )
        .get("error")
        .is_some()
    );
    let events = peer.call("events/list", json!({"taskId":task["id"]}))["events"]
        .as_array()
        .unwrap()
        .clone();
    let types: Vec<_> = events
        .iter()
        .map(|e| e["data"]["type"].as_str().unwrap())
        .collect();
    assert!(types.contains(&"agent_update") && types.contains(&"agent_approval_resolved"));
    assert_eq!(types.last(), Some(&"task_completed"));
    let final_sequence = events.last().unwrap()["sequence"].clone();
    peer.stop();
    let mut reopened = Peer::start(&directory);
    let restored = reopened.call("task/get", json!({"taskId":task["id"]}));
    assert_eq!(restored, done);
    let decision = reopened.call(
        "approval/getDecision",
        json!({"taskId":task["id"],"approvalId":approval["id"]}),
    );
    assert_eq!(decision["data"]["decision"], "allow_once");
    let receipts = reopened.call(
        "approval/getDecisions",
        json!({"approvals":[{"taskId":task["id"],"approvalId":approval["id"]}]}),
    );
    assert_eq!(
        receipts["items"],
        json!([{
            "taskId":task["id"],"approvalId":approval["id"],"decision":"allow_once",
            "sequence":decision["sequence"],"runId":decision["data"]["runId"]
        }])
    );
    assert_eq!(
        &decision,
        events
            .iter()
            .find(|e| e["data"]["type"] == "agent_approval_resolved")
            .unwrap()
    );
    assert!(
        reopened.call(
            "events/list",
            json!({"taskId":task["id"],"after":final_sequence})
        )["events"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    reopened.stop();
}

#[test]
fn waiting_agent_shutdown_expires_approvals_and_restart_never_replays() {
    let directory = Directory::new();
    let mut peer = Peer::start(&directory);
    let session = peer.session(&directory);
    let task = peer.start_task(&session, "permission", "waiting");
    let waiting = peer.wait(&task["id"], |t| t["status"] == "waiting_approval");
    let approval = waiting["agentRun"]["pendingApprovals"][0]["id"].clone();
    peer.stop();
    let mut reopened = Peer::start(&directory);
    assert_eq!(
        reopened.call("approval/listPending", json!({}))["items"],
        json!([])
    );
    let restored = reopened.call("task/get", json!({"taskId":task["id"]}));
    assert_eq!(restored["status"], "interrupted");
    assert_eq!(restored["agentRun"]["id"], waiting["agentRun"]["id"]);
    assert_eq!(restored["agentRun"]["outcomeUnknown"], true);
    assert!(
        restored["agentRun"]["pendingApprovals"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert!(
        reopened
            .request(
                "approval/respond",
                json!({"taskId":task["id"],"approvalId":approval,"decision":"allow_once"})
            )
            .get("error")
            .is_some()
    );
    assert!(
        reopened
            .request(
                "task/resume",
                json!({"taskId":task["id"],"retryUncertainModelStep":true})
            )
            .get("error")
            .is_some()
    );
    assert_eq!(
        reopened.start_task(&session, "permission", "waiting")["id"],
        task["id"]
    );
    let events = reopened.call("events/list", json!({"taskId":task["id"]}));
    assert_eq!(
        events["events"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["data"]["type"] == "agent_started")
            .count(),
        1
    );
    reopened.stop();
}

#[test]
fn agent_cancel_and_known_token_limit_keep_distinct_outcomes() {
    let directory = Directory::new();
    let mut peer = Peer::start(&directory);
    let session = peer.session(&directory);
    let task = peer.start_task(&session, "hang", "cancel");
    peer.wait(&task["id"], |t| {
        t["agentRun"]["text"]
            .as_str()
            .is_some_and(|s| s.contains("waiting"))
    });
    let before = Instant::now();
    assert_eq!(
        peer.call("task/cancel", json!({"taskId":task["id"]}))["status"],
        "cancelled"
    );
    assert!(before.elapsed() < Duration::from_secs(1));
    let cancelled = peer.wait(&task["id"], |t| t["agentRun"]["finished"] == true);
    assert_eq!(cancelled["status"], "cancelled");
    assert_eq!(cancelled["agentRun"]["outcomeUnknown"], true);
    assert!(cancelled["output"].is_null());
    let limited = peer.start_task(&session, "max_tokens", "limit");
    let limited = peer.wait(&limited["id"], |t| t["status"] == "interrupted");
    assert_eq!(limited["agentRun"]["stopReason"], "max_tokens");
    assert_eq!(limited["agentRun"]["outcomeUnknown"], false);
    peer.stop();
}

#[test]
fn persistent_grant_is_rejected_and_burst_permissions_remain_independent() {
    let directory = Directory::new();
    let mut peer = Peer::start(&directory);
    let session = peer.session(&directory);
    let task = peer.start_task(&session, "persistent", "persistent");
    let waiting = peer.wait(&task["id"], |t| t["status"] == "waiting_approval");
    let approval = waiting["agentRun"]["pendingApprovals"][0]["id"].clone();
    assert!(
        peer.request(
            "approval/respond",
            json!({"taskId":task["id"],"approvalId":approval,"decision":"allow_once"})
        )
        .get("error")
        .is_some()
    );
    peer.call(
        "approval/respond",
        json!({"taskId":task["id"],"approvalId":approval,"decision":"deny"}),
    );
    peer.wait(&task["id"], |t| t["status"] == "completed");
    let burst = peer.start_task(&session, "permissions_burst", "burst");
    let pending = peer.wait(&burst["id"], |t| {
        t["agentRun"]["pendingApprovals"]
            .as_array()
            .is_some_and(|a| a.len() == 16)
    });
    let second = &pending["agentRun"]["pendingApprovals"][1];
    assert_eq!(
        peer.call(
            "approval/get",
            json!({"taskId":burst["id"],"approvalId":second["id"]})
        ),
        json!({"kind":"agent_permission","approval":second})
    );
    let pending_items = pending["agentRun"]["pendingApprovals"].as_array().unwrap();
    let keys: Vec<_> = pending_items
        .iter()
        .rev()
        .map(|approval| json!({"taskId":burst["id"],"approvalId":approval["id"]}))
        .collect();
    let entries: Vec<_> = pending_items
        .iter()
        .rev()
        .map(|approval| json!({"kind":"agent_permission","approval":approval}))
        .collect();
    assert_eq!(
        peer.call("approval/getBatch", json!({"approvals":keys}))["items"],
        json!(entries)
    );
    for approval in pending["agentRun"]["pendingApprovals"].as_array().unwrap() {
        peer.call(
            "approval/respond",
            json!({"taskId":burst["id"],"approvalId":approval["id"],"decision":"deny"}),
        );
    }
    let done = peer.wait(&burst["id"], |t| t["status"] == "completed");
    assert_eq!(done["agentRun"]["permissionCount"], 16);
    assert!(
        done["output"]
            .as_str()
            .unwrap()
            .contains("excess_cancelled:16")
    );
    peer.stop();
}

#[test]
fn expired_permission_is_closed_and_prompt_timeout_keeps_unknown_outcome() {
    let directory = Directory::new();
    directory.timeouts(10_000, 1_000);
    let mut peer = Peer::start(&directory);
    let session = peer.session(&directory);
    let task = peer.start_task(&session, "permission", "expiry");
    let waiting = peer.wait(&task["id"], |t| t["status"] == "waiting_approval");
    let approval = waiting["agentRun"]["pendingApprovals"][0]["id"].clone();
    let done = peer.wait(&task["id"], |t| t["status"] == "completed");
    assert!(done["output"].as_str().unwrap().contains("cancelled"));
    assert_eq!(
        peer.call(
            "approval/getBatch",
            json!({"approvals":[{"taskId":task["id"],"approvalId":approval}]})
        )["items"],
        json!([null]),
    );
    assert!(
        done["agentRun"]["pendingApprovals"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert!(
        peer.request(
            "approval/respond",
            json!({"taskId":task["id"],"approvalId":approval,"decision":"allow_once"})
        )
        .get("error")
        .is_some()
    );
    let events = peer.call("events/list", json!({"taskId":task["id"]}));
    assert!(
        events["events"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["data"]["type"] == "agent_approval_closed")
    );
    peer.stop();
    directory.timeouts(100, 1_000);
    let mut peer = Peer::start(&directory);
    let hang = peer.start_task(&session, "hang", "timeout");
    let interrupted = peer.wait(&hang["id"], |t| t["status"] == "interrupted");
    assert_eq!(interrupted["agentRun"]["outcomeUnknown"], true);
    assert_eq!(
        peer.call("task/getResumeStatus", json!({"taskId":hang["id"]}))["disposition"],
        "agent_already_started"
    );
    assert!(
        peer.request(
            "task/resume",
            json!({"taskId":hang["id"],"retryUncertainModelStep":true})
        )
        .get("error")
        .is_some()
    );
    peer.stop();
}

#[test]
fn virtual_workspace_roundtrip_requires_distinct_write_and_read_approvals() {
    let directory = Directory::new();
    directory.workspace_access();
    let mut peer = Peer::start(&directory);
    let session = peer.session(&directory);
    let task = peer.start_task(&session, "host_roundtrip", "files");
    let waiting = peer.wait(&task["id"], |t| t["status"] == "waiting_approval");
    let write = &waiting["agentRun"]["pendingApprovals"][0];
    assert_eq!(write["source"], "host_tool");
    let inbox = peer.call("approval/listPending", json!({}));
    assert_eq!(inbox["items"][0]["kind"], "host_tool");
    assert_eq!(inbox["items"][0]["id"], write["id"]);
    assert_eq!(
        peer.call(
            "approval/get",
            json!({"taskId":task["id"],"approvalId":write["id"]})
        ),
        json!({"kind":"host_tool","approval":write})
    );
    let live = peer.call(
        "approval/getBatch",
        json!({"approvals":[{"taskId":task["id"],"approvalId":write["id"]}]}),
    );
    assert_eq!(
        live["items"],
        json!([{"kind":"host_tool","approval":write}])
    );
    assert_eq!(write["request"]["call"]["name"], "workspace_replace_text");
    assert_eq!(write["request"]["preview"]["createsFile"], true);
    assert!(!directory.0.join("via-agent.txt").exists());
    peer.call(
        "approval/respond",
        json!({"taskId":task["id"],"approvalId":write["id"],"decision":"allow_once"}),
    );
    let reading = peer.wait(&task["id"], |t| {
        t["agentRun"]["pendingApprovals"]
            .as_array()
            .is_some_and(|a| {
                a.iter()
                    .any(|p| p["request"]["call"]["name"] == "workspace_read_text")
            })
    });
    assert_eq!(
        fs::read_to_string(directory.0.join("via-agent.txt")).unwrap(),
        "one\n你好\nthree\n"
    );
    let read = &reading["agentRun"]["pendingApprovals"][0];
    assert_eq!(read["request"]["call"]["arguments"]["line"], 2);
    peer.call(
        "approval/respond",
        json!({"taskId":task["id"],"approvalId":read["id"],"decision":"allow_once"}),
    );
    let done = peer.wait(&task["id"], |t| t["status"] == "completed");
    let report: Value = serde_json::from_str(
        done["output"]
            .as_str()
            .unwrap()
            .strip_prefix("ready")
            .unwrap(),
    )
    .unwrap();
    assert_eq!(report["read"], "你好\n");
    assert_eq!(report["writeOk"], true);
    assert_eq!(report["directRead"], false);
    assert_eq!(done["agentRun"]["hostTools"].as_array().unwrap().len(), 2);
    assert!(
        done["agentRun"]["hostTools"]
            .as_array()
            .unwrap()
            .iter()
            .all(|op| op["status"] == "succeeded")
    );
    let events = peer.call("events/list", json!({"taskId":task["id"]}));
    assert_eq!(
        events["events"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["data"]["type"] == "agent_host_tool_started")
            .count(),
        2
    );
    peer.stop();
    let mut peer = Peer::start(&directory);
    let probe = peer.start_task(&session, "context_probe", "file-history");
    let probe = peer.wait(&probe["id"], |t| t["status"] == "completed");
    let report: Value = serde_json::from_str(
        probe["output"]
            .as_str()
            .unwrap()
            .strip_prefix("ready")
            .unwrap(),
    )
    .unwrap();
    let history = &report["context"]["turns"][0];
    assert_eq!(history["taskId"], task["id"]);
    assert_eq!(history["hostTools"], done["agentRun"]["hostTools"]);
    assert_eq!(report["context"], probe["context"]);
    assert!(history.get("pendingApprovals").is_none());
    assert!(history.get("agentRun").is_none());
    let isolated = peer.call("task/start", json!({"sessionId":session,"input":"context_probe","backend":{"type":"external_agent","agentId":"fixture"},"context":{"mode":"isolated"}}));
    let isolated = peer.wait(&isolated["id"], |t| t["status"] == "completed");
    let report: Value = serde_json::from_str(
        isolated["output"]
            .as_str()
            .unwrap()
            .strip_prefix("ready")
            .unwrap(),
    )
    .unwrap();
    assert!(report["context"].is_null());
    assert!(report["contextBytes"].is_null());
    let write = peer.start_task(&session, "host_write", "fresh-permission");
    let waiting = peer.wait(&write["id"], |t| t["status"] == "waiting_approval");
    assert_eq!(
        waiting["agentRun"]["pendingApprovals"][0]["source"],
        "host_tool"
    );
    assert!(!directory.0.join("existing.txt").exists());
    peer.call("task/cancel", json!({"taskId":write["id"]}));
    peer.stop();
}

#[test]
fn reviewed_baseline_changes_and_denial_both_preserve_host_files() {
    let directory = Directory::new();
    directory.workspace_access();
    fs::write(directory.0.join("existing.txt"), "old\n").unwrap();
    let mut peer = Peer::start(&directory);
    let session = peer.session(&directory);
    let task = peer.start_task(&session, "host_write", "stale");
    let waiting = peer.wait(&task["id"], |t| t["status"] == "waiting_approval");
    let approval = &waiting["agentRun"]["pendingApprovals"][0];
    assert!(
        approval["request"]["preview"]["diff"]
            .as_str()
            .unwrap()
            .contains("-old")
    );
    fs::write(directory.0.join("existing.txt"), "edited by user\n").unwrap();
    peer.call(
        "approval/respond",
        json!({"taskId":task["id"],"approvalId":approval["id"],"decision":"allow_once"}),
    );
    let done = peer.wait(&task["id"], |t| t["status"] == "completed");
    assert!(done["output"].as_str().unwrap().contains("writeOk:false"));
    assert_eq!(done["agentRun"]["hostTools"][0]["status"], "failed");
    assert_eq!(
        fs::read_to_string(directory.0.join("existing.txt")).unwrap(),
        "edited by user\n"
    );
    let denied = peer.start_task(&session, "host_write", "deny");
    let waiting = peer.wait(&denied["id"], |t| t["status"] == "waiting_approval");
    peer.call("approval/respond",json!({"taskId":denied["id"],"approvalId":waiting["agentRun"]["pendingApprovals"][0]["id"],"decision":"deny"}));
    let done = peer.wait(&denied["id"], |t| t["status"] == "completed");
    assert_eq!(done["agentRun"]["hostTools"][0]["status"], "denied");
    assert_eq!(
        fs::read_to_string(directory.0.join("existing.txt")).unwrap(),
        "edited by user\n"
    );
    peer.stop();
}

#[test]
fn filesystem_proxy_rejects_host_paths_and_traversal_without_disclosing_data() {
    let directory = Directory::new();
    directory.workspace_access();
    fs::write(
        directory.0.join("secret.txt"),
        "must stay private until approved",
    )
    .unwrap();
    let mut peer = Peer::start(&directory);
    let session = peer.session(&directory);
    let input = format!("host_escape {}", directory.0.join("secret.txt").display());
    let task = peer.start_task(&session, &input, "escape");
    let done = peer.wait(&task["id"], |t| t["status"] == "completed");
    let report: Value = serde_json::from_str(
        done["output"]
            .as_str()
            .unwrap()
            .strip_prefix("ready")
            .unwrap(),
    )
    .unwrap();
    assert_eq!(report["outsideRejected"], true);
    assert_eq!(report["traversalRejected"], true);
    assert!(done["agentRun"]["hostTools"].as_array().unwrap().is_empty());
    assert!(
        !done["output"]
            .as_str()
            .unwrap()
            .contains("must stay private")
    );
    peer.stop();
}

#[test]
fn shutdown_expires_host_write_consent_and_never_executes_it_on_restart() {
    let directory = Directory::new();
    directory.workspace_access();
    fs::write(directory.0.join("existing.txt"), "untouched\n").unwrap();
    let mut peer = Peer::start(&directory);
    let session = peer.session(&directory);
    let task = peer.start_task(&session, "host_write", "restart-host");
    let waiting = peer.wait(&task["id"], |t| t["status"] == "waiting_approval");
    let approval = waiting["agentRun"]["pendingApprovals"][0]["id"].clone();
    peer.stop();
    let mut reopened = Peer::start(&directory);
    let recovered = reopened.call("task/get", json!({"taskId":task["id"]}));
    assert_eq!(recovered["status"], "interrupted");
    assert_eq!(recovered["agentRun"]["hostTools"][0]["status"], "closed");
    assert!(
        recovered["agentRun"]["pendingApprovals"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert!(
        reopened
            .request(
                "approval/respond",
                json!({"taskId":task["id"],"approvalId":approval,"decision":"allow_once"})
            )
            .get("error")
            .is_some()
    );
    assert_eq!(
        fs::read_to_string(directory.0.join("existing.txt")).unwrap(),
        "untouched\n"
    );
    reopened.stop();
}
