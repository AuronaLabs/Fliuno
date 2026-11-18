//! Delegated Linux cgroup probes, run explicitly by the CI harness.
use super::*;

fn crash_server(mut peer: Peer) {
    peer.child.kill().unwrap();
    peer.child.wait().unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match peer
            .output
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
        {
            Ok(_) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                // Detach before panicking so a leaked inherited stdout handle
                // fails this test within its budget instead of hanging Drop.
                drop(peer.reader.take());
                panic!("guardian or worker retained the app-server RPC pipe");
            }
        }
    }
}

#[test]
#[ignore = "requires delegated cgroup v2; run scripts/check_linux_cgroup.py"]
fn worker_exit_reaps_a_setsid_descendant_and_applies_limits() {
    let parent = PathBuf::from(std::env::var_os("FLIUNO_TEST_CGROUP_V2_PARENT").unwrap());
    let dir = Directory::new(30000);
    let config_path = dir.root.join("terminal.json");
    let mut config: Value = serde_json::from_slice(&fs::read(&config_path).unwrap()).unwrap();
    config["presets"][0]["linuxCgroupParent"] = json!(parent);
    config["presets"][0]["resources"] = json!({
        "memoryMaxBytes": 536870912,
        "pidsMax": 128,
        "cpuQuotaMicros": 100000
    });
    fs::write(&config_path, serde_json::to_vec(&config).unwrap()).unwrap();

    let fixture = model();
    let base = fixture.url("/v1/");
    let mut peer = dir.peer(&base);
    let session = session(&mut peer, &dir);
    let started = execute(&mut peer, &session, "start", json!({}), "start");
    let terminal_id = started["terminalId"].as_str().unwrap();
    wait_text(&mut peer, &session, terminal_id, "TERMINAL_READY");

    let deadline = Instant::now() + Duration::from_secs(5);
    let group = loop {
        let groups = fs::read_dir(&parent)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
            .map(|entry| entry.path())
            .collect::<Vec<_>>();
        if let Some(group) = groups.first() {
            assert_eq!(groups.len(), 1);
            break group.clone();
        }
        assert!(
            Instant::now() < deadline,
            "terminal worker cgroup was not created"
        );
        thread::sleep(Duration::from_millis(10));
    };
    assert_eq!(
        fs::read_to_string(group.join("memory.max")).unwrap().trim(),
        "536870912"
    );
    assert_eq!(
        fs::read_to_string(group.join("pids.max")).unwrap().trim(),
        "128"
    );
    assert_eq!(
        fs::read_to_string(group.join("cpu.max")).unwrap().trim(),
        "100000 100000"
    );

    execute(
        &mut peer,
        &session,
        "write",
        json!({"terminalId":terminal_id,"data":enter("spawn-detached")}),
        "spawn-detached",
    );
    let output = wait_text(&mut peer, &session, terminal_id, "DESCENDANT ");
    let descendant_pid = text(&output)
        .lines()
        .find_map(|line| line.strip_prefix("DESCENDANT "))
        .unwrap()
        .parse::<u32>()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let proc_stat = fs::read_to_string(format!("/proc/{descendant_pid}/stat"));
        let detached = proc_stat.ok().is_some_and(|stat| {
            let Some((_, fields)) = stat.rsplit_once(')') else {
                return false;
            };
            fields.split_whitespace().nth(3) == Some(descendant_pid.to_string().as_str())
        });
        let in_group = fs::read_to_string(group.join("cgroup.procs"))
            .is_ok_and(|procs| procs.lines().any(|line| line == descendant_pid.to_string()));
        if detached && in_group {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "descendant did not join a new session and worker cgroup"
        );
        thread::sleep(Duration::from_millis(10));
    }

    execute(
        &mut peer,
        &session,
        "write",
        json!({"terminalId":terminal_id,"data":enter("exit")}),
        "exit",
    );
    let mut terminal_exited = false;
    for _ in 0..500 {
        let status = peer.call(
            "terminal/get",
            json!({"sessionId":session["id"],"terminalId":terminal_id}),
        );
        if status["status"] == "exited" {
            terminal_exited = true;
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    assert!(terminal_exited, "PTY root process did not exit");
    let deadline = Instant::now() + Duration::from_secs(10);
    while group.exists() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(20));
    }
    assert!(
        !group.exists(),
        "terminal reaper left its cgroup populated or undeleted"
    );
    let proc_stat = fs::read_to_string(format!("/proc/{descendant_pid}/stat"));
    if let Ok(stat) = proc_stat {
        let state = stat
            .rsplit_once(')')
            .and_then(|(_, fields)| fields.split_whitespace().next())
            .unwrap_or("");
        assert!(
            matches!(state, "Z" | "X"),
            "detached descendant is still alive: {state}"
        );
    }
}

#[test]
#[ignore = "requires delegated cgroup v2; run scripts/check_linux_cgroup.py"]
fn guardian_survives_app_server_sigkill_and_reaps_a_crashed_supervisors_tree() {
    let parent = PathBuf::from(std::env::var_os("FLIUNO_TEST_CGROUP_V2_PARENT").unwrap());
    let dir = Directory::new(30000);
    let config_path = dir.root.join("terminal.json");
    let mut config: Value = serde_json::from_slice(&fs::read(&config_path).unwrap()).unwrap();
    config["presets"][0]["linuxCgroupParent"] = json!(parent);
    fs::write(&config_path, serde_json::to_vec(&config).unwrap()).unwrap();
    let fixture = model();
    let base = fixture.url("/v1/");
    let mut peer = dir.peer(&base);
    let session = session(&mut peer, &dir);
    let started = execute(&mut peer, &session, "start", json!({}), "start");
    let id = started["terminalId"].as_str().unwrap();
    wait_text(&mut peer, &session, id, "TERMINAL_READY");
    execute(
        &mut peer,
        &session,
        "write",
        json!({"terminalId":id,"data":enter("spawn-detached")}),
        "spawn-detached",
    );
    let out = wait_text(&mut peer, &session, id, "DESCENDANT ");
    let descendant = text(&out)
        .lines()
        .find_map(|line| line.strip_prefix("DESCENDANT "))
        .unwrap()
        .parse::<u32>()
        .unwrap();
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
    let groups = fs::read_dir(&parent)
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().unwrap().is_dir())
        .map(|entry| entry.path())
        .collect::<Vec<_>>();
    assert_eq!(groups.len(), 1);
    let group = &groups[0];
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let stat = fs::read_to_string(format!("/proc/{descendant}/stat")).unwrap();
        let detached = stat.rsplit_once(')').unwrap().1.split_whitespace().nth(3)
            == Some(descendant.to_string().as_str());
        let members = fs::read_to_string(group.join("cgroup.procs")).unwrap();
        if detached && members.lines().any(|pid| pid == descendant.to_string()) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "descendant did not detach inside the terminal cgroup"
        );
        thread::sleep(Duration::from_millis(10));
    }
    crash_server(peer);
    let mut peer = dir.peer(&base);
    assert_eq!(
        peer.call(
            "terminal/get",
            json!({"sessionId":session["id"],"terminalId":id})
        )["status"],
        "running"
    );
    execute(
        &mut peer,
        &session,
        "write",
        json!({"terminalId":id,"data":enter("after-app-crash")}),
        "after-app-crash",
    );
    wait_text(&mut peer, &session, id, "TERMINAL_REPLY after-app-crash");
    // Remove the new server's possible reaper, too. The original guardian must
    // now do the work without any live app-server or reconnecting client.
    crash_server(peer);
    assert!(
        Command::new("/bin/kill")
            .args(["-KILL", &descriptor.supervisor_pid.to_string()])
            .status()
            .unwrap()
            .success()
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    while group.exists() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }
    assert!(
        !group.exists(),
        "independent guardian did not reclaim the terminal cgroup"
    );
    for pid in [descriptor.supervisor_pid, descendant] {
        if let Ok(stat) = fs::read_to_string(format!("/proc/{pid}/stat")) {
            let state = stat
                .rsplit_once(')')
                .unwrap()
                .1
                .split_whitespace()
                .next()
                .unwrap();
            assert!(
                matches!(state, "Z" | "X"),
                "terminal process {pid} survived: {state}"
            );
        }
    }
    let mut peer = dir.peer(&base);
    for _ in 0..3 {
        let state = peer.call(
            "terminal/get",
            json!({"sessionId":session["id"],"terminalId":id}),
        );
        assert_eq!(state["status"], "outcome_unknown");
        assert_eq!(state["supervisorConnected"], false);
        assert!(text(&output(&mut peer, &session, id)).contains("TERMINAL_REPLY after-app-crash"));
    }
    assert_eq!(
        peer.call("terminal/list", json!({"sessionId":session["id"]}))
            .as_array()
            .unwrap()
            .len(),
        1
    );
}
