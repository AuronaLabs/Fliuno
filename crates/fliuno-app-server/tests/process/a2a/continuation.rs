use super::*;

fn params(task_id: &Value, key: &str) -> Value {
    json!({"taskId":task_id,"clientRequestId":key,"input":"quarterly numbers"})
}

#[test]
fn encoded_wire_budget_failure_keeps_the_pause_available() {
    let wire = Fixture::start(|request| {
        let mut remote = task("TASK_STATE_INPUT_REQUIRED");
        remote["id"] = json!("t".repeat(256));
        remote["contextId"] = json!("c".repeat(256));
        match request.body["method"].as_str().unwrap() {
            "SendMessage" => {
                assert!(request.body["params"]["message"]["taskId"].is_null());
                send_reply(request, remote, false)
            }
            "GetTask" => response(request, remote),
            other => panic!("unexpected method {other}"),
        }
    });
    let directory = Directory::new(&wire, 10_000, false);
    let mut peer = Peer::start_with_a2a(&directory.db, &directory.config);
    let session = directory.session(&mut peer);
    let source = start(&mut peer, &session, "source");
    wait_finished(&mut peer, &source["id"]);
    // Fits the local frame and input budget. Escaping the input plus the
    // host-resolved A2A identities makes the encoded outgoing frame too large.
    let mut too_large = params(&source["id"], "large");
    too_large["input"] = json!(format!("a{}", "\0".repeat(174_680)));
    let attempt = peer.call("task/continueRemote", too_large);
    let attempt = wait_finished(&mut peer, &attempt["id"]);
    assert_eq!(attempt["status"], "failed");
    assert_eq!(attempt["agentRun"]["stopReason"], "remote_preflight_failed");
    assert_eq!(attempt["agentRun"]["outcomeUnknown"], false);
    assert_eq!(attempt["remoteContinuation"]["submissionStarted"], false);
    assert_eq!(wire.requests().len(), 2);
}

fn send_reply(request: &Request, remote: Value, streaming: bool) -> Response {
    let result = json!({"task":remote});
    if streaming {
        stream_reply(request, [result])
    } else {
        response(request, result)
    }
}

fn stream_reply(request: &Request, results: impl IntoIterator<Item = Value>) -> Response {
    Response::sse(
        results
            .into_iter()
            .map(|result| {
                format!(
                    "data: {}\n\n",
                    json!({"jsonrpc":"2.0","id":request.body["id"],"result":result})
                )
            })
            .collect(),
    )
}

#[test]
fn paused_turn_is_claimed_once_and_completed_turn_starts_a_new_remote_task() {
    for (streaming, status_first) in [(false, false), (true, false), (true, true)] {
        let send = if streaming {
            "SendStreamingMessage"
        } else {
            "SendMessage"
        };
        let (release, gate) = std::sync::mpsc::sync_channel(1);
        let gate = std::sync::Mutex::new(gate);
        let sends = AtomicUsize::new(0);
        let checks = AtomicUsize::new(0);
        let wire = Fixture::start(
            move |request| match request.body["method"].as_str().unwrap() {
                "GetTask" => {
                    assert_eq!(
                        request.body["params"],
                        json!({"id":"remote-task","historyLength":0})
                    );
                    let current = if checks.fetch_add(1, Ordering::SeqCst) == 0 {
                        gate.lock()
                            .unwrap()
                            .recv_timeout(Duration::from_secs(3))
                            .unwrap();
                        "TASK_STATE_INPUT_REQUIRED"
                    } else {
                        "TASK_STATE_COMPLETED"
                    };
                    response(request, task(current))
                }
                method if method == send => {
                    let n = sends.fetch_add(1, Ordering::SeqCst);
                    let message = &request.body["params"]["message"];
                    let mut remote = match n {
                        0 => task("TASK_STATE_INPUT_REQUIRED"),
                        1 => {
                            assert_eq!(message["taskId"], "remote-task");
                            task("TASK_STATE_COMPLETED")
                        }
                        2 => {
                            assert!(message.get("taskId").is_none());
                            let mut next = task("TASK_STATE_COMPLETED");
                            next["id"] = json!("remote-next");
                            next
                        }
                        _ => panic!("duplicate submission"),
                    };
                    if n > 0 {
                        assert_eq!(message["contextId"], "remote-context");
                        assert_eq!(message["parts"], json!([{"text":"quarterly numbers"}]));
                    }
                    if streaming && n == 0 {
                        let results = [
                            json!({"task":task("TASK_STATE_WORKING")}),
                            json!({"artifactUpdate":{"taskId":"remote-task","contextId":"remote-context",
                                "artifact":{"artifactId":"draft","parts":[{"text":"unfinished"}]},"lastChunk":false}}),
                            json!({"statusUpdate":{"taskId":"remote-task","contextId":"remote-context",
                                "status":{"state":"TASK_STATE_INPUT_REQUIRED"}}}),
                        ];
                        return stream_reply(request, results);
                    }
                    if status_first && n == 1 {
                        return stream_reply(
                            request,
                            [
                                json!({"statusUpdate":{"taskId":"remote-task","contextId":"remote-context",
                                "status":{"state":"TASK_STATE_WORKING"}}}),
                                json!({"artifactUpdate":{"taskId":"remote-task","contextId":"remote-context",
                                "artifact":{"artifactId":"report","parts":[{"text":"summary"}]},"lastChunk":true}}),
                                json!({"statusUpdate":{"taskId":"remote-task","contextId":"remote-context",
                                "status":{"state":"TASK_STATE_COMPLETED"}}}),
                            ],
                        );
                    }
                    remote["artifacts"] =
                        json!([{"artifactId":"report","parts":[{"text":"summary"}]}]);
                    send_reply(request, remote, streaming)
                }
                other => panic!("unexpected method {other}"),
            },
        );
        let directory = Directory::new(&wire, 10_000, false);
        let directory = if streaming {
            directory.with_streaming()
        } else {
            directory
        };
        let mut peer = Peer::start_with_a2a(&directory.db, &directory.config);
        let session = directory.session(&mut peer);
        let source = start(&mut peer, &session, "source");
        let source_id = &source["id"];
        let snapshot = wait_finished(&mut peer, source_id);
        assert_eq!(snapshot["agentRun"]["stopReason"], "input_required");
        let retry = params(source_id, "answer");
        let first = peer.call("task/continueRemote", retry.clone());
        wire.wait_for("GetTask");
        assert_eq!(
            peer.call("task/continueRemote", retry.clone())["id"],
            first["id"]
        );
        assert_eq!(
            peer.request("task/continueRemote", params(source_id, "competing"))["error"]["code"],
            -32009
        );
        assert_eq!(
            peer.call("task/get", json!({"taskId":first["id"]}))["remoteContinuation"]["submissionStarted"],
            false
        );
        release.send(()).unwrap();
        let completed = wait_finished(&mut peer, &first["id"]);
        assert_eq!(completed["status"], "completed");
        assert_eq!(completed["remoteContinuation"]["submissionStarted"], true);
        assert_eq!(completed["remoteContinuation"]["sourceTaskId"], *source_id);
        assert_eq!(peer.call("task/get", json!({"taskId":source_id})), snapshot);
        assert_eq!(
            peer.call("artifacts/list", json!({"taskId":source_id})),
            json!([])
        );
        assert_eq!(
            peer.call("artifacts/list", json!({"taskId":first["id"]}))
                .as_array()
                .unwrap()
                .len(),
            1
        );
        let next = peer.call("task/continueRemote", params(&first["id"], "follow-up"));
        let next = wait_finished(&mut peer, &next["id"]);
        assert_eq!(next["agentRun"]["remoteTask"]["id"], "remote-next");
        let requests = wire.requests();
        assert_eq!(requests.len(), 5);
        for (index, snapshot) in [(0, &snapshot), (2, &completed), (4, &next)] {
            assert_eq!(
                requests[index].body["params"]["message"]["messageId"],
                snapshot["agentRun"]["id"]
            );
        }
        let events = peer.call("events/list", json!({"taskId":first["id"]}));
        assert_eq!(
            events["events"][2]["data"]["type"],
            "agent_remote_continuation_sending"
        );
        drop(peer);
        let mut config: Value =
            serde_json::from_slice(&fs::read(&directory.config).unwrap()).unwrap();
        config["agents"][0]["requestTimeoutMs"] = json!(2000);
        fs::write(&directory.config, config.to_string()).unwrap();
        let mut peer = Peer::start_with_a2a(&directory.db, &directory.config);
        assert_eq!(peer.call("task/continueRemote", retry), completed);
        assert_eq!(
            peer.request("task/continueRemote", params(&first["id"], "new-key"))["error"]["code"],
            -32009
        );
        assert_eq!(wire.requests().len(), 5);
    }
}

#[test]
fn preflight_refusal_releases_claim_but_foreign_reply_does_not() {
    let checks = AtomicUsize::new(0);
    let wire = Fixture::start(
        move |request| match request.body["method"].as_str().unwrap() {
            "SendMessage" if request.body["params"]["message"]["taskId"].is_null() => {
                send_reply(request, task("TASK_STATE_INPUT_REQUIRED"), false)
            }
            "GetTask" => {
                let n = checks.fetch_add(1, Ordering::SeqCst);
                let mut remote = task(if n == 1 {
                    "TASK_STATE_WORKING"
                } else {
                    "TASK_STATE_INPUT_REQUIRED"
                });
                if n == 0 {
                    remote["contextId"] = json!("untrusted-context");
                }
                response(request, remote)
            }
            "SendMessage" => {
                let mut remote = task("TASK_STATE_COMPLETED");
                remote["id"] = json!("untrusted-task");
                remote["artifacts"] =
                    json!([{"artifactId":"untrusted","parts":[{"text":"untrusted-output"}]}]);
                send_reply(request, remote, false)
            }
            other => panic!("unexpected method {other}"),
        },
    );
    let directory = Directory::new(&wire, 10_000, false);
    let mut peer = Peer::start_with_a2a(&directory.db, &directory.config);
    let session = directory.session(&mut peer);
    let source = start(&mut peer, &session, "source");
    let id = &source["id"];
    wait_finished(&mut peer, id);
    for key in ["bad-identity", "bad-state"] {
        let attempt = peer.call("task/continueRemote", params(id, key));
        let attempt = wait_finished(&mut peer, &attempt["id"]);
        assert_eq!(attempt["status"], "failed");
        assert_eq!(attempt["agentRun"]["stopReason"], "remote_preflight_failed");
        assert_eq!(attempt["agentRun"]["outcomeUnknown"], false);
        assert_eq!(attempt["remoteContinuation"]["submissionStarted"], false);
    }
    let retry = params(id, "foreign-reply");
    let submitted = peer.call("task/continueRemote", retry.clone());
    let attempt = wait_finished(&mut peer, &submitted["id"]);
    assert_eq!(attempt["status"], "interrupted");
    assert_eq!(attempt["agentRun"]["outcomeUnknown"], true);
    assert!(attempt["agentRun"]["remoteTask"].is_null());
    assert_eq!(
        peer.call("artifacts/list", json!({"taskId":attempt["id"]})),
        json!([])
    );
    drop(peer);
    let mut peer = Peer::start_with_a2a(&directory.db, &directory.config);
    assert_eq!(peer.call("task/continueRemote", retry), attempt);
    assert_eq!(
        peer.request("task/continueRemote", params(id, "new-key"))["error"]["code"],
        -32009
    );
    assert_eq!(wire.requests().len(), 5);
}

#[test]
fn lost_stream_after_submission_is_never_replayed_on_restart() {
    let wire = Fixture::start(|request| match request.body["method"].as_str().unwrap() {
        "GetTask" => response(request, task("TASK_STATE_INPUT_REQUIRED")),
        "SendStreamingMessage" => {
            let state = if request.body["params"]["message"]["taskId"].is_null() {
                "TASK_STATE_INPUT_REQUIRED"
            } else {
                "TASK_STATE_WORKING"
            };
            send_reply(request, task(state), true)
        }
        other => panic!("unexpected method {other}"),
    });
    let directory = Directory::new(&wire, 10_000, false).with_streaming();
    let mut peer = Peer::start_with_a2a(&directory.db, &directory.config);
    let session = directory.session(&mut peer);
    let source = start(&mut peer, &session, "source");
    wait_finished(&mut peer, &source["id"]);
    let retry = params(&source["id"], "lost-stream");
    let sent = peer.call("task/continueRemote", retry.clone());
    let stopped = wait_finished(&mut peer, &sent["id"]);
    assert_eq!(stopped["agentRun"]["remoteTask"]["id"], "remote-task");
    assert_eq!(stopped["agentRun"]["outcomeUnknown"], true);
    drop(peer);
    let mut peer = Peer::start_with_a2a(&directory.db, &directory.config);
    assert_eq!(peer.call("task/continueRemote", retry), stopped);
    assert_eq!(
        peer.request("task/continueRemote", params(&source["id"], "second-send"))["error"]["code"],
        -32009
    );
    assert_eq!(wire.requests().len(), 3);
}

#[test]
fn invalid_or_missing_first_status_never_persists_identity_or_replays_submission() {
    for (task_id, context_id) in [
        ("foreign-task", "remote-context"),
        ("remote-task", "foreign-context"),
        ("remote-task", ""),
        ("", ""),
    ] {
        let wire = Fixture::start(
            move |request| match request.body["method"].as_str().unwrap() {
                "GetTask" => response(request, task("TASK_STATE_INPUT_REQUIRED")),
                "SendStreamingMessage" if request.body["params"]["message"]["taskId"].is_null() => {
                    send_reply(request, task("TASK_STATE_INPUT_REQUIRED"), true)
                }
                "SendStreamingMessage" if task_id.is_empty() => stream_reply(request, []),
                "SendStreamingMessage" => stream_reply(
                    request,
                    [json!({"statusUpdate":{
                        "taskId":task_id,"contextId":context_id,"status":{"state":"TASK_STATE_COMPLETED"}
                    }})],
                ),
                other => panic!("unexpected method {other}"),
            },
        );
        let directory = Directory::new(&wire, 10_000, false).with_streaming();
        let mut peer = Peer::start_with_a2a(&directory.db, &directory.config);
        let session = directory.session(&mut peer);
        let source = start(&mut peer, &session, "source");
        wait_finished(&mut peer, &source["id"]);
        let retry = params(&source["id"], "invalid-first-status");
        let sent = peer.call("task/continueRemote", retry.clone());
        let stopped = wait_finished(&mut peer, &sent["id"]);
        assert_eq!(stopped["status"], "interrupted");
        assert_eq!(stopped["agentRun"]["outcomeUnknown"], true);
        assert!(stopped["agentRun"]["remoteTask"].is_null());
        assert_eq!(stopped["remoteContinuation"]["submissionStarted"], true);
        drop(peer);
        let mut peer = Peer::start_with_a2a(&directory.db, &directory.config);
        assert_eq!(peer.call("task/continueRemote", retry), stopped);
        assert_eq!(
            peer.request("task/continueRemote", params(&source["id"], "another-send"))["error"]["code"],
            -32009
        );
        assert_eq!(wire.requests().len(), 3);
    }
}

#[test]
fn process_death_recovers_the_persisted_submission_boundary() {
    for killed_after_send in [false, true] {
        let wire = Fixture::start(
            move |request| match request.body["method"].as_str().unwrap() {
                "GetTask" if !killed_after_send => Response::hold(),
                "GetTask" => response(request, task("TASK_STATE_INPUT_REQUIRED")),
                "SendMessage" if request.body["params"]["message"]["taskId"].is_null() => {
                    send_reply(request, task("TASK_STATE_INPUT_REQUIRED"), false)
                }
                "SendMessage" if killed_after_send => Response::hold(),
                other => panic!("unexpected method {other}"),
            },
        );
        let directory = Directory::new(&wire, 10_000, false);
        let mut config: Value =
            serde_json::from_slice(&fs::read(&directory.config).unwrap()).unwrap();
        config["agents"][0]["requestTimeoutMs"] = json!(10_000);
        fs::write(&directory.config, config.to_string()).unwrap();
        let mut peer = Peer::start_with_a2a(&directory.db, &directory.config);
        let session = directory.session(&mut peer);
        let source = start(&mut peer, &session, "source");
        wait_finished(&mut peer, &source["id"]);
        let resume = params(&source["id"], "interrupted");
        let attempt = peer.call("task/continueRemote", resume.clone());
        let count = if killed_after_send { 3 } else { 2 };
        let deadline = Instant::now() + Duration::from_secs(5);
        while wire.requests().len() < count {
            assert!(
                Instant::now() < deadline,
                "worker did not reach the fault point"
            );
            thread::sleep(Duration::from_millis(10));
        }
        drop(peer); // Peer kills the child; there is no graceful shutdown.
        let mut peer = Peer::start_with_a2a(&directory.db, &directory.config);
        let recovered = wait_finished(&mut peer, &attempt["id"]);
        assert_eq!(recovered["status"], "interrupted");
        assert_eq!(recovered["agentRun"]["outcomeUnknown"], killed_after_send);
        assert_eq!(
            recovered["remoteContinuation"]["submissionStarted"],
            killed_after_send
        );
        assert_eq!(peer.call("task/continueRemote", resume), recovered);
        assert_eq!(wire.requests().len(), count);
        if killed_after_send {
            assert_eq!(
                peer.request("task/continueRemote", params(&source["id"], "try-again"))["error"]["code"],
                -32009
            );
        } else {
            let next = peer.call("task/continueRemote", params(&source["id"], "try-again"));
            assert_ne!(next["id"], attempt["id"]);
        }
    }
}
