use super::*;

#[test]
fn terminal_aliases_require_approval_and_own_the_native_process() {
    #[cfg(target_os = "linux")]
    let Some(parent) = std::env::var_os("FLIUNO_ACP_TEST_CGROUP_PARENT") else {
        eprintln!("isolated terminal acceptance requires FLIUNO_ACP_TEST_CGROUP_PARENT");
        return;
    };
    let directory = Directory::new();
    let path = directory.0.join("agents.json");
    let mut config: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    let executable = env!("CARGO_BIN_EXE_fliuno-app-exec-fixture");
    config["agents"][0]["args"] = json!(["--terminal"]);
    config["agents"][0]["promptTimeoutMs"] = json!(30000);
    config["agents"][0]["permissionTimeoutMs"] = json!(15000);
    config["agents"][0]["terminalPresets"] = json!([
        {"id":"render","command":executable,"args":["acp-terminal"]},
        {"id":"hold","command":executable,"args":["acp-terminal","hold"]}
    ]);
    #[cfg(target_os = "linux")]
    {
        config["agents"][0]["linuxCgroupParent"] = json!(PathBuf::from(parent));
    }
    fs::write(path, serde_json::to_vec(&config).unwrap()).unwrap();

    let mut peer = Peer::start(&directory);
    let session = peer.session(&directory);
    let approve = |peer: &mut Peer, id: &Value, decision: &str, preset: &str| {
        let waiting = peer.wait(id, |task| task["status"] == "waiting_approval");
        let approvals = waiting["agentRun"]["pendingApprovals"].as_array().unwrap();
        assert_eq!(approvals.len(), 1);
        let approval = &approvals[0];
        assert_eq!(approval["request"]["type"], "acp_terminal_create");
        assert_eq!(approval["request"]["preset"], preset);
        assert_eq!(approval["request"]["network"], "denied");
        assert_eq!(approval["request"]["program"], executable);
        assert_eq!(approval["request"]["sha256"].as_str().unwrap().len(), 64);
        let request = json!({"taskId":id,"approvalId":approval["id"],"decision":decision});
        peer.call("approval/respond", request.clone());
        assert!(
            peer.request("approval/respond", request)
                .get("error")
                .is_some()
        );
    };
    let denied = peer.start_task(&session, "terminal_normal", "deny");
    approve(&mut peer, &denied["id"], "deny", "render");
    assert_eq!(
        peer.wait(&denied["id"], |task| task["status"] == "completed")["output"],
        "terminal-denied"
    );
    let normal = peer.start_task(&session, "terminal_normal", "allow");
    approve(&mut peer, &normal["id"], "allow_once", "render");
    assert_eq!(
        peer.wait(&normal["id"], |task| task["status"] == "completed")["output"],
        "terminal-ok"
    );

    let killed = peer.start_task(&session, "terminal_kill", "kill");
    approve(&mut peer, &killed["id"], "allow_once", "hold");
    let completed = peer.wait(&killed["id"], |task| task["status"] == "completed");
    let pid = parse_pid(&completed["output"]);
    assert!(!process_running(pid));

    let cancelled = peer.start_task(&session, "terminal_cancel", "cancel");
    approve(&mut peer, &cancelled["id"], "allow_once", "hold");
    let running = peer.wait(&cancelled["id"], |task| {
        task["agentRun"]["text"]
            .as_str()
            .is_some_and(|text| text.starts_with("pid:"))
    });
    let pid = parse_pid(&running["agentRun"]["text"]);
    assert!(process_running(pid));
    peer.call("task/cancel", json!({"taskId":cancelled["id"]}));
    let stopped = peer.wait(&cancelled["id"], |task| {
        task["agentRun"]["finished"] == true
    });
    assert_eq!(stopped["status"], "cancelled");
    assert!(!process_running(pid));
    peer.stop();
}

fn parse_pid(value: &Value) -> u32 {
    value
        .as_str()
        .unwrap()
        .strip_prefix("pid:")
        .unwrap()
        .parse()
        .unwrap()
}

fn process_running(pid: u32) -> bool {
    #[cfg(windows)]
    return fliuno_sandbox::process_is_running(pid).unwrap();
    #[cfg(target_os = "linux")]
    return std::path::Path::new(&format!("/proc/{pid}")).exists();
}
