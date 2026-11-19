//! The app RPC catalog uses an independent read of the trusted peer's saved state.
use super::*;

fn enable_catalog(directory: &Directory) -> PathBuf {
    let cwd = super::persistence::trusted_config(directory);
    let path = directory.0.join("agents.json");
    let mut config: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    config["agents"][0]["sessionCatalog"] = json!(true);
    fs::write(path, serde_json::to_vec(&config).unwrap()).unwrap();
    cwd
}

fn catalog_start(peer: &mut Peer, cursor: Option<&Value>) -> Value {
    let params = match cursor {
        Some(cursor) => json!({"agentId":"fixture","cursor":cursor}),
        None => json!({"agentId":"fixture"}),
    };
    peer.call("agents/startSessionList", params)["requestId"].clone()
}

fn catalog_wait(peer: &mut Peer, request_id: &Value) -> Value {
    let deadline = Instant::now() + Duration::from_secs(25);
    loop {
        let state = peer.call("agents/getSessionList", json!({"requestId":request_id}));
        if state["status"] != "pending" {
            return state;
        }
        assert!(Instant::now() < deadline, "catalog read did not complete");
        thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn catalog_lists_saved_sessions_across_restart_and_preserves_cursor() {
    let directory = Directory::new();
    let cwd = enable_catalog(&directory);
    let mut peer = Peer::start(&directory);
    assert_eq!(
        peer.call("agents/getCapabilities", json!({"agentId":"fixture"}))["agentSessionListing"],
        true
    );
    let mut ids = Vec::new();
    for key in ["first", "second"] {
        let session = peer.session(&directory);
        let task = peer.start_task(&session, key, key);
        let completed = peer.wait(&task["id"], |task| {
            matches!(
                task["status"].as_str(),
                Some("completed" | "failed" | "interrupted")
            )
        });
        assert_eq!(completed["status"], "completed", "{completed}");
        ids.push(
            completed["agentRun"]["sessionId"]
                .as_str()
                .unwrap()
                .to_owned(),
        );
    }
    ids.sort_unstable();
    peer.stop();

    let mut peer = Peer::start(&directory);
    for params in [
        json!({"agentId":"fixture","cwd":cwd}),
        json!({"agentId":"fixture","cursor":"x".repeat(4097)}),
    ] {
        assert_eq!(
            peer.request("agents/startSessionList", params)["error"]["code"],
            -32602
        );
    }
    let id = catalog_start(&mut peer, None);
    let first = catalog_wait(&mut peer, &id);
    assert_eq!(first["status"], "complete", "{first}");
    assert_eq!(first["page"]["sessions"][0]["sessionId"], ids[0]);
    assert_eq!(
        first["page"]["sessions"][0]["title"],
        "Saved fixture session"
    );
    assert_eq!(first["page"]["nextCursor"], "next-page");
    let second_id = catalog_start(&mut peer, Some(&first["page"]["nextCursor"]));
    let second = catalog_wait(&mut peer, &second_id);
    assert_eq!(second["page"]["sessions"][0]["sessionId"], ids[1]);
    assert!(second["page"].get("nextCursor").is_none());
    for (cursor, reason) in [("unknown", "rejected"), ("oversize", "invalid_page")] {
        let id = catalog_start(&mut peer, Some(&json!(cursor)));
        assert_eq!(
            catalog_wait(&mut peer, &id),
            json!({"status":"failed","reason":reason})
        );
    }
    peer.stop();

    let mut peer = Peer::start(&directory);
    assert_eq!(
        peer.request("agents/getSessionList", json!({"requestId":id}))["error"]["code"],
        -32004
    );
    assert_eq!(fs::read_dir(cwd).unwrap().count(), 2);
}

#[test]
fn blocked_catalog_keeps_rpc_responsive_caps_queue_and_stops_on_shutdown() {
    let directory = Directory::new();
    let cwd = enable_catalog(&directory);
    let mut peer = Peer::start(&directory);
    let running = catalog_start(&mut peer, Some(&json!("hold")));
    let deadline = Instant::now() + Duration::from_secs(12);
    while !cwd.join("catalog-started").exists() {
        assert!(
            Instant::now() < deadline,
            "the peer did not receive the ACP list"
        );
        thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(
        peer.call("agents/getSessionList", json!({"requestId":running}))["status"],
        "pending"
    );
    assert_eq!(peer.call("agents/list", json!({}))[0]["id"], "fixture");
    for _ in 0..4 {
        catalog_start(&mut peer, None);
    }
    assert_eq!(
        peer.request("agents/startSessionList", json!({"agentId":"fixture"}))["error"]["code"],
        -32009
    );
    // Keep the peer stalled. EOF must interrupt it and discard the queue.
    peer.stop();
}

#[test]
fn catalog_requires_explicit_host_configuration() {
    let directory = Directory::new();
    let mut peer = Peer::start(&directory);
    assert!(
        peer.call("agents/getCapabilities", json!({"agentId":"fixture"}))
            .get("agentSessionListing")
            .is_none()
    );
    assert_eq!(
        peer.request("agents/startSessionList", json!({"agentId":"fixture"}))["error"]["code"],
        -32009
    );
}
