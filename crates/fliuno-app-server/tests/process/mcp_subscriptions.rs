use super::*;
use http_wire::{Fixture, Response, Version};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

#[test]
fn a_killed_watch_reopens_with_a_fresh_snapshot_and_never_replays() {
    let (entered, observe) = mpsc::channel();
    let stall = Arc::new(AtomicBool::new(false));
    let updated = Arc::new(AtomicBool::new(false));
    let remote_stall = Arc::clone(&stall);
    let remote_updated = Arc::clone(&updated);
    let streams = std::sync::Mutex::new(Vec::new());
    let fixture = Fixture::start(move |request| {
        if request.path == "/v1/traces" {
            return Response::json(200, json!({}));
        }
        if request.path == "/v1/responses" {
            let history = request.body["input"].as_array().unwrap();
            let output = if history
                .iter()
                .any(|item| item["type"] == "function_call_output")
            {
                json!([{"type":"message","content":[{"type":"output_text","text":"observed"}]}])
            } else {
                json!([{"type":"function_call", "call_id":"watch", "name":"mcp_remote_resources_watch_reconcile",
                    "arguments":"{\"uri\":\"fixture:guide\",\"waitMs\":100}"}])
            };
            return Response::json(200, json!({"status":"completed","output":output}));
        }
        let id = &request.body["id"];
        let result = match request.body["method"].as_str() {
            Some("server/discover") => {
                json!({"resultType":"complete","supportedVersions":["2026-07-28"],
                "ttlMs":0,"cacheScope":"private","capabilities":{"resources":{"subscribe":true}}})
            }
            Some("subscriptions/listen") => {
                assert!(!request.headers.contains_key("last-event-id"));
                let filter = &request.body["params"]["notifications"];
                assert_eq!(filter["resourceSubscriptions"], json!(["fixture:guide"]));
                let ack = json!({"jsonrpc":"2.0","method":"notifications/subscriptions/acknowledged",
                    "params":{"_meta":{"io.modelcontextprotocol/subscriptionId":id},"notifications":filter}});
                let (sender, receiver) = mpsc::channel();
                streams.lock().unwrap().push(sender);
                return Response::live_sse(format!("event: message\ndata: {ack}\n\n"), receiver);
            }
            Some("resources/read") => {
                assert_eq!(request.body["params"]["uri"], "fixture:guide");
                if remote_stall.load(Ordering::Relaxed) {
                    entered.send(()).unwrap();
                    return Response::hold();
                }
                let text = if remote_updated.load(Ordering::Relaxed) {
                    "FIXTURE_AFTER_RESTART"
                } else {
                    "FIXTURE_BEFORE_CRASH"
                };
                json!({"resultType":"complete","ttlMs":60000,"cacheScope":"private", "contents":[{"uri":"fixture:guide","text":text}]})
            }
            _ => return http_wire::reply(request, Version::Modern),
        };
        Response::json(200, json!({"jsonrpc":"2.0","id":id,"result":result}))
    });

    let root = std::env::temp_dir().join(format!("fliuno-watch-restart-{}", uuid::Uuid::new_v4()));
    fs::create_dir(&root).unwrap();
    let config = root.join("mcp.json");
    fs::write(
        &config,
        json!({"servers":[{"id":"remote","transport":"streamable_http",
        "url":fixture.url("/mcp"),"timeoutMs":15000}]})
        .to_string(),
    )
    .unwrap();
    let db = root.join("kernel.db");
    let base = fixture.url("/v1/");
    let collector = fixture.url("/v1/traces");
    let start = |trace_file: &str, otlp: bool| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_fliuno-app-server"));
        command
            .arg("--db")
            .arg(&db)
            .arg("--mcp-config")
            .arg(&config)
            .arg("--trace-log")
            .arg(root.join(trace_file))
            .env("FLIUNO_PROVIDER", "openai")
            .env("FLIUNO_MODEL", "fixture-model")
            .env("OPENAI_API_KEY", "FIXTURE_NOT_FOR_DIAGNOSTICS")
            .env("OPENAI_BASE_URL", &base)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        if otlp {
            command.arg("--otlp-endpoint").arg(&collector);
        }
        let mut peer = Peer::from_child(command.spawn().unwrap());
        peer.call(
            "initialize",
            json!({"protocolVersion":"0.1","clientName":"test"}),
        );
        peer
    };
    let mut peer = start("before.jsonl", false);
    let workspace = peer.call("workspace/open", json!({"root":root}));
    let session = peer.call("session/create", json!({"workspaceId":workspace["id"]}));
    let approve = |peer: &mut Peer| {
        let task = peer.call(
            "task/start",
            json!({"sessionId":session["id"],"input":"observe guide"}),
        );
        let id = task["id"].as_str().unwrap().to_owned();
        let waiting = peer.wait_for_status(&id, "waiting_approval");
        assert_eq!(
            waiting["pendingApproval"]["call"]["name"],
            "mcp_remote_resources_watch_reconcile"
        );
        peer.call(
            "approval/respond",
            json!({"taskId":id,
            "approvalId":waiting["pendingApproval"]["id"],"decision":"allow_once"}),
        );
        id
    };
    let baseline = approve(&mut peer);
    peer.wait_for_status(&baseline, "completed");
    stall.store(true, Ordering::Relaxed);
    let killed = approve(&mut peer);
    observe.recv_timeout(Duration::from_secs(5)).unwrap();
    peer.child.kill().unwrap();
    drop(peer);

    stall.store(false, Ordering::Relaxed);
    updated.store(true, Ordering::Relaxed);
    let mut peer = start("after.jsonl", true);
    let recovered = peer.call("task/get", json!({"taskId":killed}));
    assert_eq!(recovered["status"], "interrupted");
    let requests = |method| {
        fixture
            .requests()
            .iter()
            .filter(|r| r.body["method"] == method)
            .count()
    };
    assert_eq!(requests("subscriptions/listen"), 2);
    assert_eq!(requests("resources/read"), 2);
    let resumed = approve(&mut peer);
    let done = peer.wait_for_status(&resumed, "completed");
    let result = done["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find_map(|msg| msg["output"].get("result"))
        .unwrap();
    assert_eq!(
        result,
        &json!({"uri":"fixture:guide","status":"changed","disconnected":true})
    );
    assert_eq!(requests("subscriptions/listen"), 3);
    assert_eq!(requests("resources/read"), 3);
    let trace_records = |file| -> Vec<Value> {
        fs::read_to_string(root.join(file))
            .unwrap()
            .lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect()
    };
    let deadline = Instant::now() + Duration::from_secs(5);
    let (before, after, exported) = loop {
        let before = trace_records("before.jsonl");
        let after = trace_records("after.jsonl");
        let exported: Vec<Value> = fixture
            .requests()
            .iter()
            .filter(|r| r.path == "/v1/traces")
            .filter_map(|r| r.body.pointer("/resourceSpans/0/scopeSpans/0/spans"))
            .filter_map(Value::as_array)
            .flatten()
            .cloned()
            .collect();
        if exported.iter().any(|span| span["name"] == "mcp")
            && after.iter().any(|span| span["operation"] == "tool")
        {
            break (before, after, exported);
        }
        assert!(
            Instant::now() < deadline,
            "diagnostics did not reach both sinks"
        );
        thread::sleep(Duration::from_millis(10));
    };
    let recovery = after
        .iter()
        .find(|span| span["operation"] == "recovery")
        .unwrap();
    assert!(
        before
            .iter()
            .any(|span| span["traceId"] == recovery["traceId"])
    );
    let mcp = after
        .iter()
        .find(|span| span["operation"] == "mcp")
        .unwrap();
    assert!(after.iter().any(|span| span["operation"] == "tool"
        && span["traceId"] == mcp["traceId"]
        && span["spanId"] == mcp["parentSpanId"]));
    assert!(
        exported
            .iter()
            .any(|span| span["name"] == "mcp" && span["traceId"] == mcp["traceId"])
    );
    let emitted = json!([before, after, exported]).to_string();
    for secret in [
        "FIXTURE_NOT_FOR_DIAGNOSTICS",
        "FIXTURE_AFTER_RESTART",
        "FIXTURE_BEFORE_CRASH",
        "fixture:guide",
        "observe guide",
    ] {
        assert!(!emitted.contains(secret), "diagnostics exposed {secret}");
    }
    drop(peer);
    for file in fs::read_dir(&root).unwrap() {
        fs::remove_file(file.unwrap().path()).unwrap();
    }
    fs::remove_dir(root).unwrap();
}
