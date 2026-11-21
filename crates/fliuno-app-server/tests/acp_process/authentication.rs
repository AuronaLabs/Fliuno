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
