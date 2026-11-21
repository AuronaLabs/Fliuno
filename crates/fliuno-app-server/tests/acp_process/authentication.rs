use super::*;

fn configuration(directory: &Directory, mode: &str, method: Option<&str>) {
    let path = directory.0.join("agents.json");
    let mut config: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    config["agents"][0]["args"] = json!([mode]);
    if let Some(method) = method {
        config["agents"][0]["authenticationMethod"] = json!(method);
    }
    fs::write(path, serde_json::to_vec(&config).unwrap()).unwrap();
}

fn terminal_login_configuration(directory: &Directory, id: &str, lifetime: u64) {
    let path = directory.0.join("agents.json");
    let mut config: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    let agent = &mut config["agents"][0];
    agent["isolation"] = json!("trusted_host");
    agent["cwd"] = json!(directory.0);
    agent["terminalLogin"] = json!({
        "methodId":id, "command":env!("CARGO_BIN_EXE_fliuno-app-acp-fixture"),
        "args":["--host-login-fixture"], "envAllow":["FLIUNO_TEST_LOGIN_ALLOWED"], "lifetimeMs":lifetime
    });
    fs::write(path, serde_json::to_vec(&config).unwrap()).unwrap();
}

fn start_login(peer: &mut Peer, task: &Value) -> Value {
    peer.request(
        "agents/terminalLogin/start",
        json!({"agentId":"fixture","taskId":task["id"],"rows":24,"cols":80}),
    )
}

fn poll_login(peer: &mut Peer, id: &Value, expected: &str, needle: &str) -> Vec<u8> {
    use base64::{Engine, engine::general_purpose::STANDARD};
    let deadline = Instant::now() + Duration::from_secs(12);
    let mut cursor = 0;
    let mut bytes = Vec::new();
    loop {
        let output = peer.call(
            "agents/terminalLogin/output",
            json!({"loginId":id,"after":cursor}),
        );
        assert_eq!(output["truncated"], false);
        cursor = output["next"].as_u64().unwrap();
        bytes.extend(
            STANDARD
                .decode(output["dataBase64"].as_str().unwrap())
                .unwrap(),
        );
        if output["status"]["state"] == expected && String::from_utf8_lossy(&bytes).contains(needle)
        {
            return bytes;
        }
        assert!(
            Instant::now() < deadline,
            "login expected {expected}/{needle}, got {}: {}",
            output["status"],
            String::from_utf8_lossy(&bytes)
        );
        thread::sleep(Duration::from_millis(15));
    }
}

#[test]
fn terminal_login_is_local_and_does_not_replay_the_task_or_sensitive_input() {
    let directory = Directory::new();
    configuration(&directory, "--auth-terminal", None);
    terminal_login_configuration(&directory, "fixture-login", 15000);
    let mut peer = Peer::start(&directory);
    let session = peer.session(&directory);
    let started = peer.start_task(&session, "do not replay", "initial-login-task");
    let task = peer.wait(&started["id"], |task| task["agentRun"]["finished"] == true);
    assert_eq!(task["agentRun"]["authentication"], "terminal_required");
    let mut unsafe_request = json!({"agentId":"fixture","taskId":task["id"],"rows":24,"cols":80,"args":["peer-override"]});
    assert_eq!(
        peer.request("agents/terminalLogin/start", unsafe_request.clone())["error"]["code"],
        -32602
    );
    unsafe_request.as_object_mut().unwrap().remove("args");
    unsafe_request["agentId"] = json!("wrong-agent");
    assert_eq!(
        peer.request("agents/terminalLogin/start", unsafe_request)["error"]["code"],
        -32009
    );

    let login = start_login(&mut peer, &task)["result"]["loginId"].clone();
    assert!(login.is_string());
    assert_eq!(start_login(&mut peer, &task)["error"]["code"], -32009);
    poll_login(&mut peer, &login, "running", "host-fixture-awaiting-input");
    let input = json!({"loginId":login,"sequence":0,"data":"fixture-private-input\r"});
    assert_eq!(
        peer.call("agents/terminalLogin/input", input.clone())["nextSequence"],
        1
    );
    let repeated = peer.request("agents/terminalLogin/input", input);
    assert_eq!(repeated["error"]["code"], -32009);
    assert!(!repeated.to_string().contains("fixture-private-input"));
    poll_login(
        &mut peer,
        &login,
        "running",
        "host-fixture-awaiting-confirmation",
    );
    assert_eq!(
        peer.call(
            "agents/terminalLogin/input",
            json!({"loginId":login,"sequence":1,"data":"confirm-once\r"})
        )["nextSequence"],
        2
    );
    poll_login(&mut peer, &login, "exited", "fixture-private-output");

    let replayed = peer.start_task(&session, "do not replay", "initial-login-task");
    assert_eq!(replayed["id"], task["id"]);
    assert_eq!(replayed["agentRun"]["authentication"], "terminal_required");
    let next = peer.start_task(&session, "hello", "new-after-login");
    peer.wait(&next["id"], |task| task["status"] == "completed");
    assert_eq!(start_login(&mut peer, &next)["error"]["code"], -32009);
    let events = peer
        .call("events/list", json!({"taskId":task["id"]}))
        .to_string();
    for secret in [
        "fixture-private-input",
        "fixture-private-output",
        "host-fixture-awaiting-input",
    ] {
        assert!(!events.contains(secret));
    }
    peer.stop();

    terminal_login_configuration(&directory, "fixture-login", 14000);
    let mut peer = Peer::start(&directory);
    assert_eq!(start_login(&mut peer, &task)["error"]["code"], -32009);
    assert_eq!(
        peer.request(
            "agents/terminalLogin/output",
            json!({"loginId":login,"after":0})
        )["error"]["code"],
        -32004
    );
    peer.stop();
}

#[test]
fn terminal_login_expires_and_can_be_closed_without_input() {
    let directory = Directory::new();
    configuration(&directory, "--auth-terminal", None);
    terminal_login_configuration(&directory, "fixture-login", 2500);
    let mut peer = Peer::start(&directory);
    let session = peer.session(&directory);
    let started = peer.start_task(&session, "auth required", "login-lifetime");
    let task = peer.wait(&started["id"], |task| task["agentRun"]["finished"] == true);
    let login = start_login(&mut peer, &task)["result"]["loginId"].clone();
    poll_login(&mut peer, &login, "expired", "");
    assert!(!directory.0.join("fixture-login-complete").exists());
    let login = start_login(&mut peer, &task)["result"]["loginId"].clone();
    peer.call("agents/terminalLogin/close", json!({"loginId":login}));
    poll_login(&mut peer, &login, "closed", "");
    assert!(!directory.0.join("fixture-login-complete").exists());
    peer.stop();
}

#[test]
fn terminal_login_rejects_unsupported_duplicate_or_unconfigured_methods() {
    for (mode, method, expected) in [
        ("--auth-terminal", None, "terminal_required"),
        ("--auth-terminal", Some("different-method"), "required"),
        ("--auth-duplicate", Some("fixture-login"), "required"),
    ] {
        let directory = Directory::new();
        configuration(&directory, mode, None);
        if let Some(method) = method {
            terminal_login_configuration(&directory, method, 5000);
        }
        let mut peer = Peer::start(&directory);
        let session = peer.session(&directory);
        let started = peer.start_task(&session, "auth required", "login-method");
        let task = peer.wait(&started["id"], |task| task["agentRun"]["finished"] == true);
        assert_eq!(task["agentRun"]["authentication"], expected);
        assert_eq!(start_login(&mut peer, &task)["error"]["code"], -32009);
        peer.stop();
    }
}

fn replay(peer: &mut Peer, task: &Value, states: &[&str]) {
    let events = peer.call("events/list", json!({"taskId":task["id"]}));
    let serialized = events.to_string();
    for marker in [
        "private-auth-denial",
        "private-terminal-argument",
        "fixture-login",
    ] {
        assert!(!serialized.contains(marker));
    }
    let actual: Vec<_> = events["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|event| event["data"]["type"] == "agent_authentication_changed")
        .map(|event| event["data"]["state"].as_str().unwrap())
        .collect();
    assert_eq!(actual, states);
}

#[test]
fn host_selected_protocol_authentication_is_persisted_before_prompt() {
    let directory = Directory::new();
    configuration(&directory, "--auth-ok", Some("fixture-login"));
    let mut peer = Peer::start(&directory);
    let session = peer.session(&directory);
    let started = peer.start_task(&session, "hello", "authenticated");
    let task = peer.wait(&started["id"], |task| task["status"] == "completed");
    assert_eq!(task["agentRun"]["authentication"], "succeeded");
    assert!(task["agentRun"]["sessionId"].is_string());
    replay(&mut peer, &task, &["started", "succeeded"]);
    peer.stop();

    let mut peer = Peer::start(&directory);
    let retry = peer.start_task(&session, "hello", "authenticated");
    assert_eq!(retry["id"], task["id"]);
    replay(&mut peer, &retry, &["started", "succeeded"]);
}

#[test]
fn denied_required_and_ambiguous_authentication_never_open_a_session() {
    for (mode, method, expected) in [
        (
            "--auth-deny",
            Some("fixture-login"),
            &["started", "rejected"][..],
        ),
        ("--auth-ok", None, &["required"][..]),
        ("--auth-terminal", None, &["terminal_required"][..]),
        (
            "--auth-terminal",
            Some("fixture-login"),
            &["method_unavailable"][..],
        ),
        (
            "--auth-duplicate",
            Some("fixture-login"),
            &["method_unavailable"][..],
        ),
    ] {
        let directory = Directory::new();
        configuration(&directory, mode, method);
        let mut peer = Peer::start(&directory);
        let session = peer.session(&directory);
        let started = peer.start_task(&session, "do not dispatch", "denied");
        let task = peer.wait(&started["id"], |task| task["agentRun"]["finished"] == true);
        assert_eq!(task["status"], "interrupted", "{mode}");
        assert_eq!(task["agentRun"]["stopReason"], "auth_required", "{mode}");
        assert_eq!(task["agentRun"]["outcomeUnknown"], false, "{mode}");
        assert!(task["agentRun"]["sessionId"].is_null(), "{mode}");
        replay(&mut peer, &task, expected);
        peer.stop();
    }
}

#[test]
fn cancelling_authentication_keeps_cancellation_and_does_not_submit_prompt() {
    let directory = Directory::new();
    configuration(&directory, "--auth-hold", Some("fixture-login"));
    let mut peer = Peer::start(&directory);
    let session = peer.session(&directory);
    let started = peer.start_task(&session, "do not dispatch", "cancelled");
    let id = &started["id"];
    peer.wait(id, |task| task["agentRun"]["authentication"] == "started");
    peer.call("task/cancel", json!({"taskId":id}));
    let task = peer.wait(id, |task| task["agentRun"]["finished"] == true);
    assert_eq!(task["status"], "cancelled");
    assert!(task["agentRun"]["sessionId"].is_null());
    replay(&mut peer, &task, &["started", "cancelled"]);
    peer.stop();
}

#[test]
fn restart_records_unknown_auth_effect_but_never_replays_the_task() {
    let directory = Directory::new();
    configuration(&directory, "--auth-hold", Some("fixture-login"));
    let mut peer = Peer::start(&directory);
    let session = peer.session(&directory);
    let started = peer.start_task(&session, "do not dispatch", "disconnected");
    let id = &started["id"];
    peer.wait(id, |task| task["agentRun"]["authentication"] == "started");
    drop(peer);

    let mut peer = Peer::start(&directory);
    let recovered = peer.start_task(&session, "do not dispatch", "disconnected");
    assert_eq!(recovered["id"], *id);
    assert_eq!(recovered["status"], "interrupted");
    assert_eq!(recovered["agentRun"]["authentication"], "outcome_unknown");
    assert_eq!(recovered["agentRun"]["outcomeUnknown"], false);
    assert!(recovered["agentRun"]["sessionId"].is_null());
    replay(&mut peer, &recovered, &["started", "outcome_unknown"]);
    peer.stop();
}
