//! Actual stdio app-server -> durable core -> A2A loopback peer. No paid model,
//! external service, filesystem proxy or mocked scheduler participates.
use super::*;
use http_wire::{Fixture, Request, Response};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

static NEXT_DIRECTORY: AtomicUsize = AtomicUsize::new(0);

#[path = "a2a/continuation.rs"]
mod continuation;

struct Directory {
    root: PathBuf,
    db: PathBuf,
    config: PathBuf,
}
impl Directory {
    fn new(peer: &Fixture, task_timeout: u64, card: bool) -> Self {
        let root = std::env::temp_dir().join(format!(
            "fliuno-a2a-rpc-{}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&root).unwrap();
        let db = root.join("kernel.db");
        let config = root.join("agents.json");
        let mut agent = json!({"id":"remote","endpoint":peer.url("/rpc"),"bearerTokenEnv":"FLIUNO_TEST_A2A_TOKEN","requestTimeoutMs":1000,"taskTimeoutMs":task_timeout,"pollIntervalMs":25});
        if card {
            agent["agentCardUrl"] = json!(peer.url("/.well-known/agent-card.json"));
        }
        fs::write(&config, json!({"agents":[agent]}).to_string()).unwrap();
        Self { root, db, config }
    }
    fn with_streaming(self) -> Self {
        let mut config: Value = serde_json::from_slice(&fs::read(&self.config).unwrap()).unwrap();
        config["agents"][0]["streaming"] = json!(true);
        fs::write(&self.config, config.to_string()).unwrap();
        self
    }
    fn session(&self, peer: &mut Peer) -> String {
        let ws = peer.call("workspace/open", json!({"root":self.root}));
        peer.call("session/create", json!({"workspaceId":ws["id"]}))["id"]
            .as_str()
            .unwrap()
            .into()
    }
}
impl Drop for Directory {
    fn drop(&mut self) {
        // This directory is test-owned and never selected from peer input.
        if let (Ok(root), Ok(temp)) = (
            fs::canonicalize(&self.root),
            fs::canonicalize(std::env::temp_dir()),
        ) && root.starts_with(temp)
            && root
                .file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("fliuno-a2a-rpc-")
        {
            let _ = fs::remove_dir_all(root);
        }
    }
}
fn start(peer: &mut Peer, session: &str, key: &str) -> Value {
    peer.call("task/start", json!({"sessionId":session,"input":"prepare a report","clientRequestId":key,"backend":{"type":"external_agent","agentId":"remote"}}))
}
fn task(state: &str) -> Value {
    json!({"id":"remote-task","contextId":"remote-context","status":{"state":state}})
}
fn response(request: &Request, result: Value) -> Response {
    assert_eq!(request.method, "POST");
    assert_eq!(request.path, "/rpc");
    assert_eq!(request.headers["a2a-version"], "1.0");
    assert_eq!(
        request.headers["authorization"],
        "Bearer fixture-a2a-private"
    );
    Response::json(
        200,
        json!({"jsonrpc":"2.0","id":request.body["id"],"result":result}),
    )
}
fn wait_finished(peer: &mut Peer, id: &Value) -> Value {
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        let task = peer.call("task/get", json!({"taskId":id}));
        if task["agentRun"]["finished"] == true {
            return task;
        }
        assert!(Instant::now() < deadline, "agent did not finish: {task}");
        thread::sleep(Duration::from_millis(10));
    }
}

fn wait_checked(peer: &mut Peer, task_id: &Value, check_id: &Value) -> Value {
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        let page = peer.call("events/list", json!({"taskId":task_id}));
        if let Some(found) = page["events"].as_array().unwrap().iter().find(|event| {
            event["data"]["type"] == "agent_remote_task_checked"
                && event["data"]["checkId"] == *check_id
        }) {
            return found.clone();
        }
        assert!(
            Instant::now() < deadline,
            "remote check did not finish: {page}"
        );
        thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn remote_check_after_stream_loss_appends_evidence_without_replaying_or_changing_the_run() {
    let (release, gate) = std::sync::mpsc::sync_channel(1);
    let gate = std::sync::Mutex::new(gate);
    let wire = Fixture::start(
        move |request| match request.body["method"].as_str().unwrap() {
            "SendStreamingMessage" => Response::sse(format!(
                "data: {}\n\n",
                json!({"jsonrpc":"2.0","id":request.body["id"],"result":{"task":task("TASK_STATE_WORKING")}})
            )),
            "GetTask" => {
                assert_eq!(
                    request.body["params"],
                    json!({"id":"remote-task","historyLength":0})
                );
                gate.lock()
                    .unwrap()
                    .recv_timeout(Duration::from_secs(3))
                    .unwrap();
                let mut remote = task("TASK_STATE_COMPLETED");
                remote["artifacts"] =
                    json!([{"artifactId":"late","parts":[{"text":"private remote content"}]}]);
                response(request, remote)
            }
            other => panic!("unexpected A2A method {other}"),
        },
    );
    let directory = Directory::new(&wire, 10_000, false).with_streaming();
    let mut peer = Peer::start_with_a2a(&directory.db, &directory.config);
    let session = directory.session(&mut peer);
    let submitted = start(&mut peer, &session, "check-after-restart");
    let id = &submitted["id"];
    let before = wait_finished(&mut peer, id);
    assert_eq!(before["status"], "interrupted");
    assert_eq!(before["agentRun"]["outcomeUnknown"], true);
    drop(peer);
    // Simulate a v9 database with an existing, finished A2A run.
    rusqlite::Connection::open(&directory.db)
        .unwrap()
        .pragma_update(None, "user_version", 9)
        .unwrap();
    let mut peer = Peer::start_with_a2a(&directory.db, &directory.config);
    assert_eq!(peer.call("task/get", json!({"taskId":id})), before);
    let accepted = peer.call("task/checkRemote", json!({"taskId":id}));
    wire.wait_for("GetTask");
    assert_eq!(
        peer.request("task/checkRemote", json!({"taskId":id}))["error"]["code"],
        -32009
    );
    assert_eq!(peer.call("task/get", json!({"taskId":id})), before);
    release.send(()).unwrap();
    let event = wait_checked(&mut peer, id, &accepted["checkId"]);
    assert_eq!(
        event["data"]["outcome"],
        json!({"status":"observed","state":"completed"})
    );
    assert_eq!(event["data"]["runId"], before["agentRun"]["id"]);
    assert_eq!(peer.call("task/get", json!({"taskId":id})), before);
    assert_eq!(peer.call("artifacts/list", json!({"taskId":id})), json!([]));
    drop(peer);
    let mut peer = Peer::start_with_a2a(&directory.db, &directory.config);
    assert_eq!(wait_checked(&mut peer, id, &accepted["checkId"]), event);
    assert_eq!(wire.requests().len(), 2);
}

#[test]
fn remote_check_reports_bad_evidence_and_rejects_changed_registrations_before_io() {
    let checks = AtomicUsize::new(0);
    let wire = Fixture::start(
        move |request| match request.body["method"].as_str().unwrap() {
            "SendMessage" => response(request, json!({"task": task("TASK_STATE_COMPLETED")})),
            "GetTask" => match checks.fetch_add(1, Ordering::SeqCst) {
                0 => {
                    let mut foreign = task("TASK_STATE_COMPLETED");
                    foreign["contextId"] = json!("private-other-context");
                    response(request, foreign)
                }
                1 => response(request, task("TASK_STATE_WORKING")),
                2 => Response::hold(),
                other => panic!("unexpected check {other}"),
            },
            other => panic!("unexpected A2A method {other}"),
        },
    );
    let directory = Directory::new(&wire, 10_000, false);
    let mut peer = Peer::start_with_a2a(&directory.db, &directory.config);
    let session = directory.session(&mut peer);
    let submitted = start(&mut peer, &session, "checks");
    let id = &submitted["id"];
    let before = wait_finished(&mut peer, id);
    for status in ["identity_mismatch", "state_conflict", "unavailable"] {
        let accepted = peer.call("task/checkRemote", json!({"taskId":id}));
        let event = wait_checked(&mut peer, id, &accepted["checkId"]);
        assert_eq!(event["data"]["outcome"], json!({"status":status}));
        assert!(!event.to_string().contains("private-other-context"));
        assert_eq!(peer.call("task/get", json!({"taskId":id})), before);
    }
    drop(peer);
    let mut config: Value = serde_json::from_slice(&fs::read(&directory.config).unwrap()).unwrap();
    config["agents"][0]["requestTimeoutMs"] = json!(2000);
    fs::write(&directory.config, config.to_string()).unwrap();
    let mut peer = Peer::start_with_a2a(&directory.db, &directory.config);
    let error = peer.request("task/checkRemote", json!({"taskId":id}));
    assert_eq!(error["error"]["code"], -32009);
    assert!(
        error["error"]["message"]
            .as_str()
            .unwrap()
            .contains("registration changed")
    );
    assert_eq!(wire.requests().len(), 4);
}

#[test]
fn task_polling_artifact_parts_history_idempotency_and_restart_are_durable() {
    let sends = Arc::new(AtomicUsize::new(0));
    let polls = Arc::new(AtomicUsize::new(0));
    let sent = sends.clone();
    let polled = polls.clone();
    let wire = Fixture::start(move |request| {
        if request.method == "GET" {
            assert_eq!(request.path, "/.well-known/agent-card.json");
            return Response::json(
                200,
                json!({"supportedInterfaces":[{"url":format!("http://{}/rpc",request.headers["host"]),"protocolBinding":"JSONRPC","protocolVersion":"1.0"}],"capabilities":{}}),
            );
        }
        match request.body["method"].as_str().unwrap() {
            "SendMessage" if sent.fetch_add(1, Ordering::SeqCst) == 0 => {
                assert_eq!(
                    request.body["params"]["message"]["parts"],
                    json!([{"text":"prepare a report"}])
                );
                response(request, json!({"task":task("TASK_STATE_SUBMITTED")}))
            }
            "SendMessage" => {
                let parts = request.body["params"]["message"]["parts"]
                    .as_array()
                    .unwrap();
                assert_eq!(parts.len(), 2);
                assert!(parts[0]["text"].as_str().unwrap().contains("report text"));
                response(
                    request,
                    json!({"message":{"messageId":"next-reply","role":"ROLE_AGENT","contextId":"new-context","parts":[{"text":"follow-up"},{"data":{"answer":2}}]}}),
                )
            }
            "GetTask" => {
                assert_eq!(
                    request.body["params"],
                    json!({"id":"remote-task","historyLength":0})
                );
                let mut value = task(if polled.fetch_add(1, Ordering::SeqCst) == 0 {
                    "TASK_STATE_WORKING"
                } else {
                    "TASK_STATE_COMPLETED"
                });
                if value["status"]["state"] == "TASK_STATE_COMPLETED" {
                    value["status"]["message"] = json!({"messageId":"final-status","role":"ROLE_AGENT","taskId":"remote-task","contextId":"remote-context","parts":[{"text":"finished"}]});
                    value["artifacts"] = json!([{"artifactId":"peer-report","name":"report.json","parts":[{"text":"report text"},{"data":{"count":4}},{"raw":"aGk=","filename":"hi.txt","mediaType":"text/plain"},{"url":format!("http://{}/do-not-fetch",request.headers["host"])}],"metadata":{"vendor":"retained"}}]);
                }
                response(request, value)
            }
            other => panic!("unexpected {other}"),
        }
    });
    let directory = Directory::new(&wire, 10_000, true);
    let mut peer = Peer::start_with_a2a(&directory.db, &directory.config);
    let specs = peer.call("agents/list", json!({}));
    assert_eq!(specs[0]["protocol"], "a2a_v1_jsonrpc");
    assert_eq!(specs[0]["isolation"], "remote");
    assert_eq!(specs[0]["workspaceAccess"], false);
    let session = directory.session(&mut peer);
    let first = start(&mut peer, &session, "one");
    let completed = peer.wait_for_status(first["id"].as_str().unwrap(), "completed");
    assert_eq!(completed["output"], "finished\nreport text");
    assert_eq!(
        completed["agentRun"]["remoteTask"],
        json!({"id":"remote-task","contextId":"remote-context","state":"completed"})
    );
    assert_eq!(completed["agentRun"]["outcomeUnknown"], false);
    let artifacts = peer.call("artifacts/list", json!({"taskId":first["id"]}));
    assert_eq!(artifacts.as_array().unwrap().len(), 1);
    assert_ne!(artifacts[0]["id"], "peer-report");
    assert_eq!(
        completed["agentRun"]["artifacts"][0]["artifactId"],
        artifacts[0]["id"]
    );
    let content: Value = serde_json::from_str(artifacts[0]["content"].as_str().unwrap()).unwrap();
    assert_eq!(content["parts"][1]["data"]["count"], 4);
    assert_eq!(content["parts"][2]["raw"], "aGk=");
    assert_eq!(content["metadata"]["vendor"], "retained");
    assert_eq!(start(&mut peer, &session, "one")["id"], first["id"]);
    let second = start(&mut peer, &session, "two");
    assert_eq!(
        peer.wait_for_status(second["id"].as_str().unwrap(), "completed")["output"],
        "follow-up"
    );
    assert_eq!(sends.load(Ordering::SeqCst), 2);
    drop(peer);
    let mut peer = Peer::start_with_a2a(&directory.db, &directory.config);
    assert_eq!(start(&mut peer, &session, "one")["id"], first["id"]);
    assert_eq!(
        peer.call("artifacts/list", json!({"taskId":first["id"]})),
        artifacts
    );
    let events = peer.call("events/list", json!({"taskId":first["id"]}));
    assert_eq!(
        events["events"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["data"]["type"] == "agent_remote_task_updated")
            .count(),
        3
    );
    assert_eq!(sends.load(Ordering::SeqCst), 2);
    assert!(!events.to_string().contains("fixture-a2a-private"));
}

#[test]
fn streaming_status_and_chunked_artifacts_are_persisted_without_polling() {
    let wire = Fixture::start(|request| {
        assert_eq!(request.body["method"], "SendStreamingMessage");
        assert_eq!(request.headers["accept"], "text/event-stream");
        let id = request.body["id"].as_str().unwrap();
        let results = [
            json!({"task":task("TASK_STATE_WORKING")}),
            json!({"artifactUpdate":{"taskId":"remote-task","contextId":"remote-context","artifact":{"artifactId":"stream-report","name":"report.txt","parts":[{"text":"stream "}]},"append":false,"lastChunk":false}}),
            json!({"artifactUpdate":{"taskId":"remote-task","contextId":"remote-context","artifact":{"artifactId":"stream-report","parts":[{"text":"result"}]},"append":true,"lastChunk":true}}),
            json!({"statusUpdate":{"taskId":"remote-task","contextId":"remote-context","status":{"state":"TASK_STATE_COMPLETED","message":{"messageId":"stream-final","taskId":"remote-task","contextId":"remote-context","role":"ROLE_AGENT","parts":[{"text":"done"}]}}}}),
        ];
        let body: String = results
            .into_iter()
            .map(|result| {
                format!(
                    "data: {}\n\n",
                    json!({"jsonrpc":"2.0","id":id,"result":result})
                )
            })
            .collect();
        Response::sse(body)
    });
    let directory = Directory::new(&wire, 10_000, false).with_streaming();
    let mut peer = Peer::start_with_a2a(&directory.db, &directory.config);
    let session = directory.session(&mut peer);
    let submitted = start(&mut peer, &session, "streaming");
    let completed = wait_finished(&mut peer, &submitted["id"]);
    assert_eq!(completed["status"], "completed");
    assert_eq!(completed["output"], "done\nstream \nresult");
    assert_eq!(
        completed["agentRun"]["remoteTask"],
        json!({"id":"remote-task","contextId":"remote-context","state":"completed"})
    );
    assert_eq!(completed["agentRun"]["outcomeUnknown"], false);
    let stored = peer.call("artifacts/list", json!({"taskId":submitted["id"]}));
    assert_eq!(stored.as_array().unwrap().len(), 1);
    let raw: Value = serde_json::from_str(stored[0]["content"].as_str().unwrap()).unwrap();
    assert_eq!(raw["artifactId"], "stream-report");
    assert_eq!(raw["parts"], json!([{"text":"stream "},{"text":"result"}]));
    let requests = wire.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].body["method"], "SendStreamingMessage");
}

#[test]
fn incomplete_stream_is_unknown_and_is_never_replayed_or_polled() {
    let wire = Fixture::start(|request| {
        assert_eq!(request.body["method"], "SendStreamingMessage");
        let id = request.body["id"].as_str().unwrap();
        Response::sse(format!(
            "data: {}\n\n",
            json!({"jsonrpc":"2.0","id":id,"result":{"task":task("TASK_STATE_WORKING")}})
        ))
    });
    let directory = Directory::new(&wire, 10_000, false).with_streaming();
    let mut peer = Peer::start_with_a2a(&directory.db, &directory.config);
    let session = directory.session(&mut peer);
    let submitted = start(&mut peer, &session, "stream-interrupted");
    let interrupted = wait_finished(&mut peer, &submitted["id"]);
    assert_eq!(interrupted["status"], "interrupted");
    assert_eq!(interrupted["agentRun"]["outcomeUnknown"], true);
    let requests = wire.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].body["method"], "SendStreamingMessage");
}

#[test]
fn remote_failures_and_pauses_have_distinct_stop_reasons_and_no_false_success() {
    for (state, reason, local) in [
        ("TASK_STATE_FAILED", "failed", "failed"),
        ("TASK_STATE_REJECTED", "rejected", "failed"),
        ("TASK_STATE_INPUT_REQUIRED", "input_required", "interrupted"),
        ("TASK_STATE_AUTH_REQUIRED", "auth_required", "interrupted"),
    ] {
        let wire = Fixture::start(move |request| response(request, json!({"task":task(state)})));
        let directory = Directory::new(&wire, 10_000, false);
        let mut peer = Peer::start_with_a2a(&directory.db, &directory.config);
        let session = directory.session(&mut peer);
        let first = start(&mut peer, &session, "one");
        let stopped = wait_finished(&mut peer, &first["id"]);
        assert_eq!(stopped["status"], local);
        assert_eq!(stopped["agentRun"]["stopReason"], reason);
        assert_eq!(stopped["agentRun"]["outcomeUnknown"], false);
        assert!(
            peer.call("session/context", json!({"sessionId":session}))["turns"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        assert_eq!(wire.requests().len(), 1);
    }
}

#[test]
fn cancelling_known_remote_task_preserves_acknowledgement_or_uncertainty() {
    for acknowledged in [true, false] {
        let wire = Fixture::start(
            move |request| match request.body["method"].as_str().unwrap() {
                "SendMessage" => response(request, json!({"task":task("TASK_STATE_WORKING")})),
                "GetTask" => Response::hold(),
                "CancelTask" => response(
                    request,
                    task(if acknowledged {
                        "TASK_STATE_CANCELED"
                    } else {
                        "TASK_STATE_WORKING"
                    }),
                ),
                other => panic!("unexpected {other}"),
            },
        );
        let directory = Directory::new(&wire, 10_000, false);
        let mut peer = Peer::start_with_a2a(&directory.db, &directory.config);
        let session = directory.session(&mut peer);
        let first = start(&mut peer, &session, "one");
        wire.wait_for("GetTask");
        assert_eq!(
            peer.call("task/cancel", json!({"taskId":first["id"]}))["status"],
            "cancelled"
        );
        let stopped = wait_finished(&mut peer, &first["id"]);
        assert_eq!(stopped["status"], "cancelled");
        assert_eq!(stopped["agentRun"]["outcomeUnknown"], !acknowledged);
        assert_eq!(
            stopped["agentRun"]["remoteTask"]["state"],
            if acknowledged { "cancelled" } else { "working" }
        );
        assert_eq!(
            wire.requests()
                .iter()
                .filter(|r| r.body["method"] == "CancelTask")
                .count(),
            1
        );
        drop(peer);
        let mut peer = Peer::start_with_a2a(&directory.db, &directory.config);
        assert_eq!(
            peer.call("task/get", json!({"taskId":first["id"]}))["agentRun"],
            stopped["agentRun"]
        );
    }
}

#[test]
fn deadline_uses_bounded_remote_cancel_and_requires_terminal_evidence() {
    let wire = Fixture::start(|request| match request.body["method"].as_str().unwrap() {
        "SendMessage" => response(request, json!({"task":task("TASK_STATE_WORKING")})),
        "GetTask" => Response::hold(),
        "CancelTask" => response(request, task("TASK_STATE_CANCELED")),
        other => panic!("unexpected {other}"),
    });
    let directory = Directory::new(&wire, 200, false);
    let mut peer = Peer::start_with_a2a(&directory.db, &directory.config);
    let session = directory.session(&mut peer);
    let first = start(&mut peer, &session, "one");
    let stopped = wait_finished(&mut peer, &first["id"]);
    assert_eq!(stopped["status"], "cancelled");
    assert_eq!(stopped["agentRun"]["outcomeUnknown"], false);
    assert_eq!(
        wire.requests()
            .iter()
            .filter(|r| r.body["method"] == "SendMessage")
            .count(),
        1
    );
    assert_eq!(
        wire.requests()
            .iter()
            .filter(|r| r.body["method"] == "CancelTask")
            .count(),
        1
    );
}

#[test]
fn accepted_submission_without_reply_is_not_replayed_after_process_restart() {
    let wire = Fixture::start(|_| Response::hold());
    let directory = Directory::new(&wire, 10_000, false);
    let mut peer = Peer::start_with_a2a(&directory.db, &directory.config);
    let session = directory.session(&mut peer);
    let first = start(&mut peer, &session, "one");
    wire.wait_for("SendMessage");
    drop(peer); // Hard process loss after acceptance, before the task ID arrived.
    let mut peer = Peer::start_with_a2a(&directory.db, &directory.config);
    let recovered = peer.call("task/get", json!({"taskId":first["id"]}));
    assert_eq!(recovered["status"], "interrupted");
    assert_eq!(recovered["agentRun"]["finished"], true);
    assert_eq!(recovered["agentRun"]["outcomeUnknown"], true);
    assert_eq!(start(&mut peer, &session, "one")["id"], first["id"]);
    assert!(
        peer.request("task/resume", json!({"taskId":first["id"]}))
            .get("error")
            .is_some()
    );
    assert_eq!(wire.requests().len(), 1);
}

#[test]
fn poll_identity_change_is_uncertain_and_never_imports_another_tasks_artifact() {
    let wire = Fixture::start(|request| {
        if request.body["method"] == "SendMessage" {
            return response(request, json!({"task":task("TASK_STATE_WORKING")}));
        }
        let mut changed = task("TASK_STATE_COMPLETED");
        changed["id"] = json!("different-task");
        changed["artifacts"] = json!([{"artifactId":"wrong","parts":[{"text":"wrong task"}]}]);
        response(request, changed)
    });
    let directory = Directory::new(&wire, 10_000, false);
    let mut peer = Peer::start_with_a2a(&directory.db, &directory.config);
    let session = directory.session(&mut peer);
    let first = start(&mut peer, &session, "one");
    let stopped = wait_finished(&mut peer, &first["id"]);
    assert_eq!(stopped["status"], "interrupted");
    assert_eq!(stopped["agentRun"]["outcomeUnknown"], true);
    assert_eq!(stopped["agentRun"]["remoteTask"]["id"], "remote-task");
    assert!(
        peer.call("artifacts/list", json!({"taskId":first["id"]}))
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert_eq!(wire.requests().len(), 2);
}

#[test]
fn poll_state_regression_is_uncertain_and_does_not_overwrite_remote_state() {
    let polls = Arc::new(AtomicUsize::new(0));
    let polled = polls.clone();
    let wire = Fixture::start(
        move |request| match request.body["method"].as_str().unwrap() {
            "SendMessage" => response(request, json!({"task":task("TASK_STATE_WORKING")})),
            "GetTask" => {
                assert_eq!(
                    polled.fetch_add(1, Ordering::SeqCst),
                    0,
                    "a regressed state must stop polling"
                );
                response(
                    request,
                    json!({"id":"remote-task","contextId":"remote-context","status":{"state":"TASK_STATE_SUBMITTED"}}),
                )
            }
            other => panic!("unexpected A2A method {other}"),
        },
    );
    let directory = Directory::new(&wire, 10_000, false);
    let mut peer = Peer::start_with_a2a(&directory.db, &directory.config);
    let session = directory.session(&mut peer);
    let submitted = start(&mut peer, &session, "poll-regression");
    let interrupted = wait_finished(&mut peer, &submitted["id"]);
    assert_eq!(interrupted["status"], "interrupted");
    assert_eq!(interrupted["agentRun"]["outcomeUnknown"], true);
    assert_eq!(interrupted["agentRun"]["remoteTask"]["state"], "working");
    assert_eq!(polls.load(Ordering::SeqCst), 1);
    assert_eq!(wire.requests().len(), 2);
}

#[test]
fn completion_racing_remote_cancel_is_observed_without_overwriting_local_cancel() {
    let polls = Arc::new(AtomicUsize::new(0));
    let count = polls.clone();
    let wire = Fixture::start(
        move |request| match request.body["method"].as_str().unwrap() {
            "SendMessage" => response(request, json!({"task":task("TASK_STATE_WORKING")})),
            "GetTask" if count.fetch_add(1, Ordering::SeqCst) == 0 => Response::hold(),
            "CancelTask" => Response::json(
                200,
                json!({"jsonrpc":"2.0","id":request.body["id"],"error":{"code":-32002,"message":"Task cannot be canceled"}}),
            ),
            "GetTask" => {
                let mut result = task("TASK_STATE_COMPLETED");
                result["artifacts"] =
                    json!([{"artifactId":"late","parts":[{"text":"late report"}]}]);
                response(request, result)
            }
            other => panic!("unexpected {other}"),
        },
    );
    let directory = Directory::new(&wire, 10_000, false);
    let mut peer = Peer::start_with_a2a(&directory.db, &directory.config);
    let session = directory.session(&mut peer);
    let first = start(&mut peer, &session, "one");
    wire.wait_for("GetTask");
    peer.call("task/cancel", json!({"taskId":first["id"]}));
    let stopped = wait_finished(&mut peer, &first["id"]);
    assert_eq!(stopped["status"], "cancelled");
    assert_eq!(stopped["agentRun"]["outcomeUnknown"], false);
    assert_eq!(stopped["agentRun"]["remoteTask"]["state"], "completed");
    assert!(
        peer.call("artifacts/list", json!({"taskId":first["id"]}))
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert_eq!(polls.load(Ordering::SeqCst), 2);
    assert_eq!(
        wire.requests()
            .iter()
            .filter(|r| r.body["method"] == "CancelTask")
            .count(),
        1
    );
}
