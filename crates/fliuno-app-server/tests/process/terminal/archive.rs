use super::*;

fn wait_owner_exit(dir: &std::path::Path) {
    let owner = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(dir.join("owner.lock"))
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if owner.try_lock().is_ok() {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "supervisor did not release owner lock"
        );
        thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn terminal_retention_requires_host_opt_in_and_preserves_closed_output_cursor() {
    let dir = Directory::new(30000);
    let fixture = model();
    let base = fixture.url("/v1/");
    let mut peer = dir.peer(&base);
    let session = session(&mut peer, &dir);
    let start = execute(&mut peer, &session, "start", json!({}), "retention-start");
    let id = start["terminalId"].as_str().unwrap();
    let terminal = dir.service.as_ref().unwrap().directory(id).unwrap();
    wait_text(&mut peer, &session, id, "TERMINAL_READY");
    execute(
        &mut peer,
        &session,
        "write",
        json!({"terminalId":id,"data":enter("exit")}),
        "retention-exit",
    );
    wait_text(&mut peer, &session, id, "TERMINAL_EXIT");
    wait_owner_exit(&terminal);
    let before = output(&mut peer, &session, id);
    assert_eq!(before["snapshot"]["outputClosed"], true);
    drop(peer);

    // Backdate the fixture's expiry without modifying wall clock or PTY state.
    {
        let db = rusqlite::Connection::open(terminal.join("output.db")).unwrap();
        let raw: String = db
            .query_row("SELECT data FROM snapshot", [], |r| r.get(0))
            .unwrap();
        let mut snapshot: Value = serde_json::from_str(&raw).unwrap();
        snapshot["expiresAtUnixMs"] = json!(1);
        db.execute("UPDATE snapshot SET data=?1", [snapshot.to_string()])
            .unwrap();
    }
    let mut peer = dir.peer(&base);
    assert_eq!(
        output(&mut peer, &session, id)["dataBase64"],
        before["dataBase64"]
    );
    assert!(terminal.is_dir());
    drop(peer);

    let config_path = dir.root.join("terminal.json");
    let mut config: Value = serde_json::from_slice(&fs::read(&config_path).unwrap()).unwrap();
    config["retention"] = json!({"archiveAfterExpiryDays":1});
    fs::write(config_path, config.to_string()).unwrap();
    let mut peer = dir.peer(&base);
    let archived = output(&mut peer, &session, id);
    assert_eq!(archived["snapshot"]["status"], "exited");
    assert_eq!(archived["snapshot"]["outputClosed"], true);
    assert!(archived["snapshot"]["archivedAtUnixMs"].is_u64());
    assert_eq!(archived["next"], before["next"]);
    assert_eq!(archived["gap"], true);
    assert_eq!(archived["dataBase64"], "");
    assert!(!terminal.exists());
    drop(peer);

    let mut peer = dir.peer(&base);
    assert_eq!(output(&mut peer, &session, id), archived);
}

#[test]
fn terminal_archive_requires_approval_preserves_tombstone_and_survives_server_restart() {
    let dir = Directory::new(30000);
    let fixture = model();
    let base = fixture.url("/v1/");
    let mut peer = dir.peer(&base);
    let session = session(&mut peer, &dir);
    let other = super::session(&mut peer, &dir);
    let start = execute(&mut peer, &session, "start", json!({}), "start");
    let id = start["terminalId"].as_str().unwrap();
    let terminal = dir.service.as_ref().unwrap().directory(id).unwrap();
    wait_text(&mut peer, &session, id, "TERMINAL_READY");
    assert!(
        peer.request(
            "terminal/archive",
            json!({"sessionId":session["id"],"terminalId":id})
        )
        .get("error")
        .is_some()
    );
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
        json!({"terminalId":id,"data":enter("exit")}),
        "exit",
    );
    wait_text(&mut peer, &session, id, "TERMINAL_EXIT");
    wait_owner_exit(&terminal);
    let denied = propose(
        &mut peer,
        &session,
        "archive",
        json!({"terminalId":id}),
        "denied-archive",
    );
    assert!(terminal.join("output.db").is_file());
    peer.call("approval/respond", json!({"taskId":denied["id"],"approvalId":denied["pendingApproval"]["id"],"decision":"deny"}));
    assert!(terminal.join("output.db").is_file());
    assert!(
        peer.request(
            "terminal/archive",
            json!({"sessionId":other["id"],"terminalId":id})
        )
        .get("error")
        .is_some()
    );
    let archive = execute(
        &mut peer,
        &session,
        "archive",
        json!({"terminalId":id}),
        "archive",
    );
    assert_eq!(archive["cleanupPending"], false);
    assert!(archive["snapshot"]["archivedAtUnixMs"].is_u64());
    assert!(!terminal.exists());
    assert_eq!(
        peer.call("terminal/list", json!({"sessionId":session["id"]})),
        json!([])
    );
    let archives = peer.call(
        "terminal/archives",
        json!({"sessionId":session["id"],"limit":1}),
    );
    assert_eq!(archives["items"], json!([archive]));
    assert_eq!(archives["hasMore"], false);
    assert_eq!(
        peer.call("terminal/archives", json!({"sessionId":other["id"]}))["items"],
        json!([])
    );
    let output = super::output(&mut peer, &session, id);
    assert_eq!(output["gap"], true);
    assert_eq!(output["dataBase64"], "");
    assert_eq!(output["next"], archive["snapshot"]["endOffset"]);
    assert_eq!(
        peer.call(
            "terminal/tail",
            json!({"sessionId":session["id"],"terminalId":id})
        ),
        output
    );
    assert!(
        peer.request(
            "terminal/tail",
            json!({"sessionId":other["id"],"terminalId":id})
        )["error"]
            .is_object()
    );
    assert!(
        peer.request(
            "terminal/tail",
            json!({"sessionId":session["id"],"terminalId":id,"limit":32769})
        )["error"]
            .is_object()
    );
    let subscription = subscriptions::subscribe(&mut peer, &session, id, 0, 32768);
    assert_eq!(
        subscriptions::notification(&mut peer, &subscription, "notifications/terminalOutput")["output"]
            ["gap"],
        true
    );
    assert_eq!(
        subscriptions::notification(
            &mut peer,
            &subscription,
            "notifications/terminalSubscriptionEnded"
        )["reason"],
        "archived"
    );
    drop(peer);
    let mut peer = dir.peer(&base);
    assert_eq!(
        peer.call(
            "terminal/get",
            json!({"sessionId":session["id"],"terminalId":id})
        ),
        archive["snapshot"]
    );
    assert_eq!(
        peer.call(
            "terminal/archive",
            json!({"sessionId":session["id"],"terminalId":id})
        ),
        archive
    );
    let next = execute(&mut peer, &session, "start", json!({}), "next-terminal");
    assert_ne!(next["terminalId"], id);
    let next_id = next["terminalId"].as_str().unwrap();
    wait_text(&mut peer, &session, next_id, "TERMINAL_READY");
    execute(
        &mut peer,
        &session,
        "release",
        json!({"terminalId":next_id}),
        "release",
    );
}

fn worker(bootstrap: &[u8]) -> Child {
    let mut command = Command::new(env!("CARGO_BIN_EXE_fliuno-app-server"));
    command
        .arg("--terminal-worker")
        .env_clear()
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000);
    }
    let mut child = command.spawn().unwrap();
    child.stdin.take().unwrap().write_all(bootstrap).unwrap();
    child
}
fn stop_worker(child: &mut Child) -> std::process::ExitStatus {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("terminal worker did not stop");
        }
        thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn terminal_archive_commits_before_cleanup_and_delayed_native_worker_cannot_relaunch_it() {
    let dir = Directory::new(30000);
    let fixture = model();
    let base = fixture.url("/v1/");
    let mut peer = dir.peer(&base);
    let session = session(&mut peer, &dir);
    let session_id = session["id"].as_str().unwrap();
    let reserve = |id: &str| {
        fliuno_terminal::fixture::reserve_delayed_worker(
            dir.service.as_ref().unwrap(),
            session_id,
            id,
            &dir.root,
            std::path::Path::new(env!("CARGO_BIN_EXE_fliuno-app-exec-fixture")),
            vec!["terminal-fixture".into()],
        )
        .unwrap()
    };
    // Positive control uses the same bootstrap builder and launches a real PTY.
    let control = "a".repeat(64);
    let bootstrap = reserve(&control);
    let mut child = worker(&bootstrap);
    wait_text(&mut peer, &session, &control, "TERMINAL_READY");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime
        .block_on(dir.service.as_ref().unwrap().call(
            session_id,
            &control,
            Operation::Release {
                operation_id: "e".repeat(64),
            },
        ))
        .unwrap();
    assert!(stop_worker(&mut child).success());

    let id = "b".repeat(64);
    let bootstrap = reserve(&id);
    let directory = dir.service.as_ref().unwrap().directory(&id).unwrap();
    let marker = directory.join("preserve-host-file");
    fs::write(&marker, b"preserve").unwrap();
    let archived = peer.call(
        "terminal/archive",
        json!({"sessionId":session_id,"terminalId":id}),
    );
    assert_eq!(archived["cleanupPending"], true);
    assert_eq!(archived["snapshot"]["status"], "outcome_unknown");
    assert!(directory.join("output.db").is_file());
    // Original Starting snapshot and spool still exist, so only the ledger
    // barrier prevents this valid delayed bootstrap from launching the command.
    let mut delayed = worker(&bootstrap);
    assert!(!stop_worker(&mut delayed).success());
    let mut diagnostic = String::new();
    delayed
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut diagnostic)
        .unwrap();
    assert!(diagnostic.contains("identity was archived"), "{diagnostic}");
    assert!(!directory.join("endpoint.json").exists());
    assert_eq!(fs::read(&marker).unwrap(), b"preserve");
    drop(peer);
    fs::remove_file(marker).unwrap();
    let mut peer = dir.peer(&base);
    assert!(!directory.exists());
    let records = peer.call("terminal/archives", json!({"sessionId":session_id}));
    assert_eq!(records["items"][0]["cleanupPending"], false);
    assert_eq!(records["items"][0]["snapshot"], archived["snapshot"]);
}

#[test]
fn terminal_archive_batch_reports_partial_results_and_retries_after_real_supervisor_exit() {
    let dir = Directory::new(30000);
    let fixture = model();
    let base = fixture.url("/v1/");
    let mut peer = dir.peer(&base);
    let session = session(&mut peer, &dir);
    let other = super::session(&mut peer, &dir);
    let reserve = |id: &str, session: &Value| {
        fliuno_terminal::fixture::reserve_delayed_worker(
            dir.service.as_ref().unwrap(),
            session["id"].as_str().unwrap(),
            id,
            &dir.root,
            std::path::Path::new(env!("CARGO_BIN_EXE_fliuno-app-exec-fixture")),
            vec!["terminal-fixture".into()],
        )
        .unwrap();
    };
    let first = "1".repeat(64);
    let foreign = "2".repeat(64);
    let missing = "3".repeat(64);
    let pending = "4".repeat(64);
    reserve(&first, &session);
    reserve(&foreign, &other);
    reserve(&pending, &session);
    let pending_dir = dir.service.as_ref().unwrap().directory(&pending).unwrap();
    let marker = pending_dir.join("preserve-host-file");
    fs::write(&marker, b"preserve").unwrap();
    let started = execute(
        &mut peer,
        &session,
        "start",
        json!({}),
        "batch-live-control",
    );
    let live = started["terminalId"].as_str().unwrap().to_owned();
    let live_dir = dir.service.as_ref().unwrap().directory(&live).unwrap();
    wait_text(&mut peer, &session, &live, "TERMINAL_READY");
    for ids in [json!([first, "../invalid-later-ID"]), json!([first, first])] {
        assert!(
            peer.request(
                "terminal/archiveBatch",
                json!({"sessionId":session["id"],"terminalIds":ids})
            )
            .get("error")
            .is_some()
        );
        assert!(
            dir.service
                .as_ref()
                .unwrap()
                .directory(&first)
                .unwrap()
                .join("output.db")
                .is_file()
        );
        assert!(
            peer.call("terminal/archives", json!({"sessionId":session["id"]}))["items"]
                .as_array()
                .unwrap()
                .is_empty()
        );
    }
    let ids = json!([first, live, foreign, missing, pending]);
    let result = peer.call(
        "terminal/archiveBatch",
        json!({"sessionId":session["id"],"terminalIds":ids}),
    );
    let items = result["items"].as_array().unwrap();
    assert_eq!(items.len(), 5);
    for (index, id) in ids.as_array().unwrap().iter().enumerate() {
        assert_eq!(items[index]["terminalId"], *id);
        assert_eq!(
            items[index]["status"],
            if matches!(index, 0 | 4) {
                "archived"
            } else {
                "failed"
            }
        );
    }
    assert_eq!(items[0]["archive"]["snapshot"]["status"], "outcome_unknown");
    assert_eq!(items[0]["archive"]["cleanupPending"], false);
    assert_eq!(items[4]["archive"]["cleanupPending"], true);
    assert_eq!(
        peer.call(
            "terminal/get",
            json!({"sessionId":session["id"],"terminalId":live})
        )["status"],
        "running"
    );
    assert_eq!(fs::read(&marker).unwrap(), b"preserve");
    execute(
        &mut peer,
        &session,
        "release",
        json!({"terminalId":live}),
        "batch-release-control",
    );
    wait_owner_exit(&live_dir);
    drop(peer);
    fs::remove_file(marker).unwrap();
    let mut peer = dir.peer(&base);
    let retry = peer.call(
        "terminal/archiveBatch",
        json!({"sessionId":session["id"],"terminalIds":ids}),
    );
    assert_eq!(
        retry["items"][0]["archive"]["snapshot"],
        items[0]["archive"]["snapshot"]
    );
    assert_eq!(retry["items"][1]["status"], "archived");
    assert_eq!(retry["items"][2]["status"], "failed");
    assert_eq!(retry["items"][3]["status"], "failed");
    assert_eq!(retry["items"][4]["archive"]["cleanupPending"], false);
    assert!(!live_dir.exists());
    assert!(!pending_dir.exists());
    assert!(
        dir.service
            .as_ref()
            .unwrap()
            .directory(&foreign)
            .unwrap()
            .join("output.db")
            .is_file()
    );
    assert_eq!(
        peer.call("terminal/archives", json!({"sessionId":session["id"]}))["items"]
            .as_array()
            .unwrap()
            .len(),
        3
    );
}
