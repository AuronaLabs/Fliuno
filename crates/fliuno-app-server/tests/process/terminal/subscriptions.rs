use super::*;

pub(super) fn subscribe(
    peer: &mut Peer,
    session: &Value,
    terminal: &str,
    after: u64,
    limit: usize,
) -> String {
    peer.call(
        "terminal/subscribe",
        json!({"sessionId":session["id"], "terminalId":terminal, "after":after, "limit":limit}),
    )["subscriptionId"]
        .as_str()
        .unwrap()
        .into()
}
pub(super) fn notification(peer: &mut Peer, id: &str, method: &str) -> Value {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(index) = peer
            .notifications
            .iter()
            .position(|n| n["method"] == method && n["params"]["subscriptionId"] == id)
        {
            return peer.notifications.remove(index)["params"].clone();
        }
        let value = peer
            .output
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .unwrap_or_else(|error| {
                panic!(
                    "{method} missing for {id}: {error}; {:?}",
                    peer.notifications
                )
            });
        peer.notifications.push(value);
    }
}
fn until(peer: &mut Peer, id: &str, cursor: &mut u64, needle: &str, limit: u64) -> Vec<u8> {
    let mut bytes = Vec::new();
    while !String::from_utf8_lossy(&bytes).contains(needle) {
        let n = notification(peer, id, "notifications/terminalOutput");
        let output = &n["output"];
        assert_eq!(output["from"], *cursor, "{output}");
        assert_eq!(output["gap"], false);
        let chunk = base64::engine::general_purpose::STANDARD
            .decode(output["dataBase64"].as_str().unwrap())
            .unwrap();
        assert!(chunk.len() as u64 <= limit);
        *cursor += chunk.len() as u64;
        assert_eq!(output["next"], *cursor);
        bytes.extend(chunk);
    }
    bytes
}

#[test]
fn terminal_subscriptions_deliver_live_bytes_and_resume_after_server_restart() {
    let dir = Directory::new(30000);
    let fixture = model();
    let base = fixture.url("/v1/");
    let mut peer = dir.peer(&base);
    let session = session(&mut peer, &dir);
    let start = execute(&mut peer, &session, "start", json!({}), "start");
    let id = start["terminalId"].as_str().unwrap();
    wait_text(&mut peer, &session, id, "TERMINAL_READY");
    let tail = peer.call(
        "terminal/tail",
        json!({"sessionId":session["id"],"terminalId":id,"limit":8}),
    );
    let mut cursor = tail["next"].as_u64().unwrap();
    assert_eq!(tail["from"], cursor - 8);
    assert_eq!(tail["snapshot"]["endOffset"], cursor);
    assert_eq!(tail["snapshot"]["supervisorConnected"], false);
    assert_eq!(tail["gap"], true);
    let page = peer.call(
        "terminal/output",
        json!({"sessionId":session["id"],"terminalId":id,"after":tail["from"],"limit":8}),
    );
    assert_eq!(page["dataBase64"], tail["dataBase64"]);
    let subscription = subscribe(&mut peer, &session, id, cursor, 31);
    execute(
        &mut peer,
        &session,
        "write",
        json!({"terminalId":id,"data":enter("订阅中文测试")}),
        "first",
    );
    until(
        &mut peer,
        &subscription,
        &mut cursor,
        "TERMINAL_REPLY 订阅中文测试",
        31,
    );
    assert_eq!(
        peer.call(
            "terminal/unsubscribe",
            json!({"subscriptionId":subscription})
        )["removed"],
        true
    );
    assert_eq!(
        peer.call(
            "terminal/unsubscribe",
            json!({"subscriptionId":subscription})
        )["removed"],
        false
    );
    // Earlier frames may already have been written before the unsubscribe
    // response; after it, both queued and future observations are suppressed.
    peer.notifications.clear();
    execute(
        &mut peer,
        &session,
        "write",
        json!({"terminalId":id,"data":enter("after-unsubscribe")}),
        "second",
    );
    wait_text(&mut peer, &session, id, "TERMINAL_REPLY after-unsubscribe");
    assert!(
        !peer
            .notifications
            .iter()
            .any(|n| n["params"]["subscriptionId"] == subscription)
    );
    let transient = subscribe(&mut peer, &session, id, cursor, 32768);
    drop(peer);
    let mut peer = dir.peer(&base);
    assert_eq!(
        peer.call("terminal/unsubscribe", json!({"subscriptionId":transient}))["removed"],
        false
    );
    let resumed = subscribe(&mut peer, &session, id, cursor, 32768);
    assert_ne!(resumed, subscription);
    until(
        &mut peer,
        &resumed,
        &mut cursor,
        "TERMINAL_REPLY after-unsubscribe",
        32768,
    );
    execute(
        &mut peer,
        &session,
        "write",
        json!({"terminalId":id,"data":enter("exit")}),
        "exit",
    );
    until(&mut peer, &resumed, &mut cursor, "TERMINAL_EXIT", 32768);
    let end = notification(
        &mut peer,
        &resumed,
        "notifications/terminalSubscriptionEnded",
    );
    assert_eq!(end["reason"], "closed");
    assert!(end["after"].as_u64().unwrap() >= cursor);
    assert_eq!(end["terminalId"], id);
}

#[test]
fn rust_terminal_feed_resumes_from_consumed_tail_after_app_server_restart() {
    use fliuno_client::{Client, TerminalFeedEvent};
    use fliuno_protocol::{
        TerminalOutputParams, TerminalSubscriptionEndReason, TerminalTailParams,
    };

    struct Server(Child);
    impl Drop for Server {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let dir = Directory::new(30000);
    let fixture = model();
    let base = fixture.url("/v1/");
    let mut peer = dir.peer(&base);
    let session = session(&mut peer, &dir);
    let terminal = execute(&mut peer, &session, "start", json!({}), "start");
    let id = terminal["terminalId"].as_str().unwrap();
    wait_text(&mut peer, &session, id, "TERMINAL_READY");
    drop(peer);

    let connect = || {
        let mut server = Server(
            Command::new(env!("CARGO_BIN_EXE_fliuno-app-server"))
                .arg("--db")
                .arg(dir.root.join("kernel.db"))
                .arg("--terminal-config")
                .arg(dir.root.join("terminal.json"))
                .env("FLIUNO_PROVIDER", "demo")
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .spawn()
                .unwrap(),
        );
        let stdin = server.0.stdin.take().unwrap();
        let stdout = server.0.stdout.take().unwrap();
        let client =
            Client::connect(BufReader::new(stdout), stdin, "rust-terminal-test", |_| {}).unwrap();
        (server, client)
    };
    let tail_params = TerminalTailParams {
        session_id: session["id"].as_str().unwrap().into(),
        terminal_id: id.into(),
        limit: 8,
    };
    let (server, mut client) = connect();
    let tail = client.terminal_tail(&tail_params, |_| {}).unwrap();
    assert!(!tail.snapshot.supervisor_connected);
    assert_eq!(tail.next, tail.snapshot.end_offset);
    assert_eq!(tail.bytes.len(), 8);
    let page = client
        .terminal_output(
            &TerminalOutputParams {
                session_id: tail_params.session_id.clone(),
                terminal_id: id.into(),
                after: tail.from,
                limit: 8,
            },
            |_| {},
        )
        .unwrap();
    assert_eq!(page.bytes, tail.bytes);
    drop(client);
    drop(server);

    let mut peer = dir.peer(&base);
    execute(
        &mut peer,
        &session,
        "write",
        json!({"terminalId":id,"data":enter("exit")}),
        "exit",
    );
    wait_text(&mut peer, &session, id, "TERMINAL_EXIT");
    drop(peer);

    let (_server, mut client) = connect();
    let mut feed = client
        .subscribe_terminal(
            &TerminalOutputParams {
                session_id: tail_params.session_id,
                terminal_id: id.into(),
                after: tail.next,
                limit: 7,
            },
            |_| {},
        )
        .unwrap();
    let mut rebuilt = Vec::new();
    loop {
        let note = client.read_notification().unwrap();
        match feed.observe(note).unwrap() {
            TerminalFeedEvent::Output(page) => {
                assert!(!page.gap);
                assert_eq!(page.from, tail.next + rebuilt.len() as u64);
                rebuilt.extend(page.bytes);
            }
            TerminalFeedEvent::Ended(end) => {
                assert_eq!(end.reason, TerminalSubscriptionEndReason::Closed);
                assert_eq!(end.after, tail.next + rebuilt.len() as u64);
                break;
            }
            TerminalFeedEvent::Unrelated(_) => {}
        }
    }
    assert!(
        rebuilt
            .windows(b"TERMINAL_EXIT".len())
            .any(|bytes| bytes == b"TERMINAL_EXIT")
    );
    assert!(
        !client
            .unsubscribe_terminal(feed.subscription_id(), |_| {})
            .unwrap()
    );
}

#[test]
fn terminal_subscriptions_bound_resources_reject_ownership_and_replay_retention_gaps() {
    let dir = Directory::new(30000);
    let fixture = model();
    let base = fixture.url("/v1/");
    let mut peer = dir.peer(&base);
    let session = session(&mut peer, &dir);
    let other = super::session(&mut peer, &dir);
    let start = execute(&mut peer, &session, "start", json!({}), "start");
    let id = start["terminalId"].as_str().unwrap();
    let ready = wait_text(&mut peer, &session, id, "TERMINAL_READY");
    let after = ready["snapshot"]["endOffset"].as_u64().unwrap();
    for params in [
        json!({"sessionId":other["id"],"terminalId":id}),
        json!({"sessionId":session["id"],"terminalId":id,"after":u64::MAX}),
        json!({"sessionId":session["id"],"terminalId":id,"limit":0}),
        json!({"sessionId":session["id"],"terminalId":id,"after":after + 1000000}),
        json!({"sessionId":session["id"],"terminalId":id,"limit":32769}),
    ] {
        assert!(
            peer.request("terminal/subscribe", params)
                .get("error")
                .is_some()
        )
    }
    let subscriptions: Vec<_> = (0..8)
        .map(|_| subscribe(&mut peer, &session, id, after, 32768))
        .collect();
    assert!(
        peer.request(
            "terminal/subscribe",
            json!({"sessionId":session["id"],"terminalId":id})
        )
        .get("error")
        .is_some()
    );
    for subscription in subscriptions {
        assert_eq!(
            peer.call(
                "terminal/unsubscribe",
                json!({"subscriptionId":subscription})
            )["removed"],
            true
        );
    }
    execute(
        &mut peer,
        &session,
        "write",
        json!({"terminalId":id,"data":enter("flood")}),
        "flood",
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let status = peer.call(
            "terminal/get",
            json!({"sessionId":session["id"],"terminalId":id}),
        );
        if status["endOffset"].as_u64().unwrap() > 256 * 1024 {
            break;
        }
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(10));
    }
    let replay = subscribe(&mut peer, &session, id, 0, 32768);
    let n = notification(&mut peer, &replay, "notifications/terminalOutput");
    let out = &n["output"];
    assert_eq!(out["gap"], true);
    assert!(out["from"].as_u64().unwrap() > 0);
    let mut cursor = out["next"].as_u64().unwrap();
    until(&mut peer, &replay, &mut cursor, "FLOOD_END", 32768);
    peer.call("terminal/unsubscribe", json!({"subscriptionId":replay}));
    // Supervisor capacity is shared across facades and leaves short status /
    // control requests usable even when every observation slot is occupied.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let service = dir.service.as_ref().unwrap();
        let session_id = session["id"].as_str().unwrap();
        let mut observers = Vec::new();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        // The previous unsubscribe abort is asynchronous. Wait boundedly for
        // its socket closure to release the cross-process observation permit.
        while observers.len() < 8 {
            match service.subscribe(session_id, id, cursor, 32768).await {
                Ok(observer) => observers.push(observer),
                Err(error) => {
                    assert!(error.contains("subscription limit"), "{error}");
                    assert!(tokio::time::Instant::now() < deadline);
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            }
        }
        match service.subscribe(session_id, id, cursor, 32768).await {
            Err(error) => assert!(error.contains("subscription limit"), "{error}"),
            Ok(_) => panic!("supervisor accepted excess observers"),
        }
        assert!(
            service
                .status(session_id, id)
                .await
                .unwrap()
                .supervisor_connected
        );
        drop(observers);
    });
    execute(
        &mut peer,
        &session,
        "release",
        json!({"terminalId":id}),
        "release",
    );
}
