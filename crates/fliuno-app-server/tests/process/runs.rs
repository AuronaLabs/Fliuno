use super::*;

#[test]
fn hard_exit_recovers_the_queued_run_child_without_replaying_the_sent_child() {
    let root = std::env::temp_dir().join(format!(
        "fliuno-run-hard-exit-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir(&root).unwrap();
    let db = root.join("kernel.db");
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}/v1/", listener.local_addr().unwrap());
    let mut peer = Peer::start_with_model_fixture(&db, Some(&base));
    let workspace = peer.call("workspace/open", json!({"root":root}));
    let session = peer.call("session/create", json!({"workspaceId":workspace["id"]}));
    let mut occupied = Vec::new();
    // Leave exactly one of the four model workers free.
    for index in 0..3 {
        peer.call(
            "task/start",
            json!({"sessionId":session["id"],"input":format!("occupy-{index}")}),
        );
        occupied.push(accept_request(&listener));
    }
    let run = peer.call(
        "run/start",
        json!({"sessionId":session["id"], "clientRequestId":"hard-exit",
        "nodes":[{"nodeId":"a","input":"first"},{"nodeId":"b","input":"second"}]}),
    );
    occupied.push(accept_request(&listener));
    let mut sent = None;
    let mut queued = None;
    for node in run["nodes"].as_array().unwrap() {
        let id = node["taskId"].as_str().unwrap();
        let task = peer.call("task/get", json!({"taskId":id}));
        match task["modelSteps"].as_u64().unwrap() {
            0 => assert!(queued.replace(id).is_none()),
            1 => assert!(sent.replace(id).is_none()),
            other => panic!("unexpected attempt count: {other}"),
        }
    }
    let (queued, sent) = (queued.unwrap(), sent.unwrap());
    drop(peer); // Kill the process without running its graceful shutdown path.
    drop(occupied);

    let mut peer = Peer::start_with_model_fixture(&db, Some(&base));
    let request = accept_request(&listener);
    complete_request(request, "continued after restart");
    let finished = peer.wait_for_status(queued, "completed");
    assert_eq!(finished["output"], "continued after restart");
    assert_eq!(finished["modelSteps"], 1);
    let uncertain = peer.call("task/get", json!({"taskId":sent}));
    assert_eq!(uncertain["status"], "interrupted");
    assert_eq!(uncertain["modelSteps"], 1);
    assert_eq!(uncertain["modelOutcomeUnknown"], true);
    let run_id = &run["id"];
    let recovered = peer.call("run/get", json!({"runId":run_id}));
    assert_eq!(recovered["status"], "interrupted");
    let events = peer.call("run/events", json!({"runId":run_id}));
    let started = events["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|event| event["data"]["type"] == "node_started")
        .count();
    assert_eq!(started, 2);
    assert_eq!(
        listener.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
    drop(peer);
    for entry in fs::read_dir(&root).unwrap() {
        fs::remove_file(entry.unwrap().path()).unwrap();
    }
    fs::remove_dir(root).unwrap();
}

#[test]
fn run_deadline_closes_approval_without_run_polling_and_survives_restart() {
    let root = std::env::temp_dir().join(format!(
        "fliuno-run-deadline-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir(&root).unwrap();
    let db = root.join("kernel.db");
    let mut peer = Peer::start(&db);
    let workspace = peer.call("workspace/open", json!({"root":root}));
    let session = peer.call("session/create", json!({"workspaceId":workspace["id"]}));
    let request = json!({"sessionId":session["id"],"clientRequestId":"timed",
    "timeoutMs":1500,"maxConcurrency":1,"nodes":[
        {"nodeId":"first","input":"approval"},
        {"nodeId":"next","input":"no dispatch","dependsOn":["first"]}
    ]});
    let run = peer.call("run/start", request.clone());
    let task_id = run["nodes"][0]["taskId"].as_str().unwrap();
    let waiting = peer.wait_for_status(task_id, "waiting_approval");
    assert_eq!(peer.wait_for_status(task_id, "cancelled")["modelSteps"], 1);
    let run_id = run["id"].as_str().unwrap();
    let first = peer.call("run/events", json!({"runId":run_id}));
    let events = first["events"].as_array().unwrap();
    assert_eq!(
        events.last().unwrap()["data"],
        json!({"type":"timed_out",
        "deadlineAtUnixMs":run["deadlineAtUnixMs"]})
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| event["data"]["type"] == "node_started")
            .count(),
        1
    );
    let rejected = peer.request(
        "approval/respond",
        json!({"taskId":task_id,
        "approvalId":waiting["pendingApproval"]["id"],"decision":"allow_once"}),
    );
    assert!(rejected.get("error").is_some(), "{rejected}");
    drop(peer);

    let mut peer = Peer::start(&db);
    let retried = peer.call("run/start", request);
    assert_eq!(retried["id"], run["id"]);
    assert_eq!(retried["deadlineAtUnixMs"], run["deadlineAtUnixMs"]);
    assert_eq!(retried["status"], "timed_out");
    assert_eq!(retried["nodes"][1]["status"], "timed_out");
    assert!(retried["nodes"][1].get("taskId").is_none());
    assert_eq!(first, peer.call("run/events", json!({"runId":run_id})));
    drop(peer);
    fs::remove_file(&db).unwrap();
    // SQLite sidecars or the database writer lock may remain after a hard stop.
    for entry in fs::read_dir(&root).unwrap() {
        fs::remove_file(entry.unwrap().path()).unwrap();
    }
    fs::remove_dir(root).unwrap();
}
