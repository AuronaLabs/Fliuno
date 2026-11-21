//! Durable ACP sessions across owned native processes.
use super::*;

pub(super) fn trusted_config(directory: &Directory) -> PathBuf {
    let path = directory.0.join("state");
    fs::create_dir(&path).unwrap();
    let mut agent = json!({
        "id":"fixture", "command":env!("CARGO_BIN_EXE_fliuno-app-acp-fixture"),
        "args":["--persistent-fixture"], "isolation":"trusted_host", "cwd":path,
        "promptTimeoutMs":30000
    });
    if cfg!(windows) {
        agent["resources"] = json!({"memoryMaxBytes":67108864,"pidsMax":1});
    }
    #[cfg(target_os = "linux")]
    if let Some(parent) = std::env::var_os("FLIUNO_ACP_TEST_CGROUP_PARENT") {
        agent["linuxCgroupParent"] = json!(PathBuf::from(parent));
        // The Linux pids controller accounts for library and runtime threads.
        agent["resources"] = json!({"memoryMaxBytes":268435456,"pidsMax":16});
    }
    fs::write(
        directory.0.join("agents.json"),
        serde_json::to_vec(&json!({"agents":[agent]})).unwrap(),
    )
    .unwrap();
    path
}

#[test]
fn isolated_session_restores_only_its_checkpoint_after_hard_restart() {
    #[cfg(target_os = "linux")]
    let Some(parent) = std::env::var_os("FLIUNO_ACP_TEST_CGROUP_PARENT") else {
        eprintln!(
            "isolated state acceptance requires delegated cgroup v2 (FLIUNO_ACP_TEST_CGROUP_PARENT)"
        );
        return;
    };
    let directory = Directory::new();
    let state_root = directory.0.join("private-state");
    fs::create_dir(&state_root).unwrap();
    let mut config = json!({"agents":[{
        "id":"fixture", "command":env!("CARGO_BIN_EXE_fliuno-app-acp-fixture"),
        "args":["--persistent-fixture"], "isolation":"native_scratch",
        "sessionStateDirectory":state_root, "promptTimeoutMs":30000
    }]});
    #[cfg(target_os = "linux")]
    {
        config["agents"][0]["linuxCgroupParent"] = json!(PathBuf::from(parent));
    }
    let config_path = directory.0.join("agents.json");
    fs::write(&config_path, serde_json::to_vec(&config).unwrap()).unwrap();
    let mut peer = Peer::start(&directory);
    let open = |peer: &mut Peer, name: &str| {
        let workspace = directory.0.join(name);
        fs::create_dir(&workspace).unwrap();
        let opened = peer.call("workspace/open", json!({"root":workspace}));
        peer.call("session/create", json!({"workspaceId":opened["id"]}))["id"]
            .as_str()
            .unwrap()
            .to_owned()
    };
    let first = open(&mut peer, "first-workspace");
    let other = open(&mut peer, "other-workspace");
    let first_task = peer.start_task(&first, "alpha", "first");
    let first = peer.wait(&first_task["id"], |task| task["status"] != "running");
    assert_eq!(first["status"], "completed", "{first}");
    let other_task = peer.start_task(&other, "unrelated", "first");
    let other = peer.wait(&other_task["id"], |task| task["status"] != "running");
    assert_eq!(other["status"], "completed", "{other}");
    assert_eq!(other["output"], "unrelated");
    let agent_session = &first["agentRun"]["sessionId"];
    assert_ne!(other["agentRun"]["sessionId"], *agent_session);
    let scopes: Vec<_> = fs::read_dir(&state_root)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    assert_eq!(scopes.len(), 2);
    for scope in &scopes {
        assert_eq!(fs::read_dir(scope).unwrap().count(), 1);
    }
    drop(peer);

    let mut peer = Peer::start(&directory);
    let request = json!({"taskId":first["id"],"input":"beta","clientRequestId":"continuation"});
    let draft = peer.call("task/continueAgentSession", request.clone());
    let second = peer.wait(&draft["id"], |task| task["status"] != "running");
    assert_eq!(second["status"], "completed", "{second}");
    assert_eq!(second["agentRun"]["sessionId"], *agent_session);
    assert_eq!(second["output"], "alpha | beta");
    assert_eq!(
        peer.call("task/continueAgentSession", request)["id"],
        second["id"]
    );
    let interrupted_request =
        json!({"taskId":second["id"],"input":"lose_response","clientRequestId":"interrupted"});
    let interrupted = peer.call("task/continueAgentSession", interrupted_request.clone());
    let running = peer.wait(&interrupted["id"], |t| {
        t["agentRun"]["text"] == "prompt stored"
    });
    assert_eq!(
        running["agentSessionContinuation"]["submissionStarted"],
        true
    );
    drop(peer);

    let mut peer = Peer::start(&directory);
    let interrupted = peer.call("task/get", json!({"taskId":interrupted["id"]}));
    assert_eq!(interrupted["status"], "interrupted");
    assert_eq!(interrupted["agentRun"]["outcomeUnknown"], true);
    assert_eq!(
        peer.call("task/continueAgentSession", interrupted_request)["id"],
        interrupted["id"]
    );
    assert_eq!(
        scopes
            .iter()
            .map(|scope| fs::read_dir(scope).unwrap().count())
            .sum::<usize>(),
        3
    );
    drop(peer);

    config["agents"][0]["promptTimeoutMs"] = json!(29000);
    fs::write(&config_path, serde_json::to_vec(&config).unwrap()).unwrap();
    let mut peer = Peer::start(&directory);
    let changed = peer.request(
        "task/continueAgentSession",
        json!({
            "taskId":other["id"],"input":"wrong registration","clientRequestId":"changed"
        }),
    );
    assert_eq!(changed["error"]["code"], -32009);
    peer.stop();
}

#[test]
fn resuming_a_session_after_restart_never_replays_an_uncertain_turn() {
    let directory = Directory::new();
    let cwd = trusted_config(&directory);
    let mut peer = Peer::start(&directory);
    let session = peer.session(&directory);
    let source = peer.start_task(&session, "first", "first");
    let source = peer.wait(&source["id"], |task| task["status"] == "completed");
    let agent_session = source["agentRun"]["sessionId"].as_str().unwrap();
    let state_file = cwd.join(format!("fixture-{agent_session}.json"));
    assert_eq!(
        peer.call("agents/getCapabilities", json!({"agentId":"fixture"})),
        json!({"remoteTaskChecks":false,"remoteTaskWatch":false,
            "remoteTaskContinuation":false,"agentSessionContinuation":true})
    );
    drop(peer);

    let mut peer = Peer::start(&directory);
    let request = json!({"taskId":source["id"],"input":"second","clientRequestId":"second"});
    let draft = peer.call("task/continueAgentSession", request.clone());
    assert_eq!(draft["sessionId"], session);
    let second = peer.wait(&draft["id"], |task| task["status"] == "completed");
    assert_eq!(second["agentRun"]["sessionId"], agent_session);
    assert_eq!(second["output"], "first | second");
    assert_eq!(
        second["agentSessionContinuation"]["submissionStarted"],
        true
    );
    assert_eq!(
        peer.call("task/continueAgentSession", request)["id"],
        second["id"]
    );
    assert_eq!(
        peer.request(
            "task/continueAgentSession",
            json!({
                "taskId":source["id"],"input":"fork","clientRequestId":"fork"
            })
        )["error"]["code"],
        -32009
    );

    let third_request =
        json!({"taskId":second["id"],"input":"lose_response","clientRequestId":"third"});
    let draft = peer.call("task/continueAgentSession", third_request.clone());
    // Observe the remote side effect before killing the server mid-request.
    // The fixture deliberately never acknowledges this prompt.
    let expected = json!(["first", "second", "lose_response"]);
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let observed = fs::read(&state_file)
            .ok()
            .and_then(|body| serde_json::from_slice::<Value>(&body).ok());
        if observed.as_ref() == Some(&expected) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the Agent did not record its third prompt"
        );
        thread::sleep(Duration::from_millis(10));
    }
    let in_flight = peer.call("task/get", json!({"taskId":draft["id"]}));
    assert_eq!(in_flight["status"], "running");
    assert_eq!(
        in_flight["agentSessionContinuation"]["submissionStarted"],
        true
    );
    drop(peer);

    let mut peer = Peer::start(&directory);
    let interrupted = peer.call("task/get", json!({"taskId":draft["id"]}));
    assert_eq!(interrupted["status"], "interrupted");
    assert_eq!(interrupted["agentRun"]["outcomeUnknown"], true);
    assert_eq!(
        interrupted["agentSessionContinuation"]["submissionStarted"],
        true
    );
    assert_eq!(
        peer.call("task/continueAgentSession", third_request)["id"],
        interrupted["id"]
    );
    assert_eq!(
        peer.request(
            "task/continueAgentSession",
            json!({
                "taskId":second["id"],"input":"try again","clientRequestId":"fourth"
            })
        )["error"]["code"],
        -32009
    );
    let observed: Value = serde_json::from_slice(&fs::read(state_file).unwrap()).unwrap();
    assert_eq!(observed, json!(["first", "second", "lose_response"]));
}

#[test]
fn a_rejected_resume_releases_the_source_claim_and_never_opens_a_new_session() {
    let directory = Directory::new();
    let cwd = trusted_config(&directory);
    let mut peer = Peer::start(&directory);
    let session = peer.session(&directory);
    let draft = peer.start_task(&session, "refuse_resume", "begin");
    let source = peer.wait(&draft["id"], |task| task["status"] == "completed");
    for key in ["retry-1", "retry-2"] {
        let attempt = peer.call(
            "task/continueAgentSession",
            json!({
                "taskId":source["id"],"input":"no prompt expected","clientRequestId":key
            }),
        );
        let failed = peer.wait(&attempt["id"], |task| task["status"] == "failed");
        assert_eq!(
            failed["agentSessionContinuation"]["submissionStarted"],
            false
        );
        assert_eq!(failed["agentRun"]["sessionId"], Value::Null);
    }
    assert_eq!(fs::read_dir(&cwd).unwrap().count(), 1);
    drop(peer);
    directory.timeouts(1600, 5000);
    let mut peer = Peer::start(&directory);
    assert_eq!(
        peer.request(
            "task/continueAgentSession",
            json!({
                "taskId":source["id"],"input":"registration changed","clientRequestId":"changed"
            })
        )["error"]["code"],
        -32009
    );
}
