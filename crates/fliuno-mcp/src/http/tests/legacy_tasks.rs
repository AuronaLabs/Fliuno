use super::*;
use fliuno_core::{DemoProvider, Kernel};
use std::fs;

#[test]
fn streamable_http_preserves_legacy_tasks_over_json_and_sse() {
    for sse in [false, true] {
        let polls = std::sync::atomic::AtomicUsize::new(0);
        let fixture = Fixture::start(move |request| {
            if request.method != "POST" {
                return wire::reply(request, Version::Legacy("2025-11-25"));
            }
            let mut snapshot = json!({"taskId":"legacy-job","status":"working",
                "createdAt":"2026-11-21T00:00:00Z","lastUpdatedAt":"2026-11-21T00:00:00Z",
                "ttl":null,"pollInterval":1234});
            let params = &request.body["params"];
            let method = request.body["method"].as_str().unwrap();
            let mut notice = None;
            let result = match method {
                "initialize" => {
                    assert_eq!(params["capabilities"]["tasks"], json!({}));
                    assert!(params["capabilities"].get("extensions").is_none());
                    json!({"protocolVersion":"2025-11-25", "capabilities":{
                        "tools":{},"tasks":{"requests":{"tools":{"call":{}}},"cancel":{}}
                    }, "serverInfo":{"name":"legacy-http-fixture","version":"1"}})
                }
                "tools/list" => json!({"tools":[{"name":"legacy",
                    "inputSchema":{"type":"object"},"outputSchema":{"type":"null"},
                    "execution":{"taskSupport":"required"}}]}),
                "tools/call" => {
                    assert_eq!(params["task"], json!({}));
                    json!({"task":snapshot})
                }
                "tasks/get" => {
                    assert_eq!(params["taskId"], "legacy-job");
                    if sse && polls.fetch_add(1, Ordering::Relaxed) == 0 {
                        let mut changed = snapshot.clone();
                        changed["status"] = json!("completed");
                        notice = Some(
                            json!({"jsonrpc":"2.0","method":"notifications/tasks/status","params":changed}),
                        );
                    } else {
                        snapshot["status"] = json!("completed");
                    }
                    snapshot
                }
                "tasks/result" => {
                    assert_eq!(params["taskId"], "legacy-job");
                    json!({"content":[],"structuredContent":null,
                        "_meta":{"io.modelcontextprotocol/related-task":{"taskId":"legacy-job"}}})
                }
                _ => return wire::reply(request, Version::Legacy("2025-11-25")),
            };
            let message = json!({"jsonrpc":"2.0","id":request.body["id"],"result":result});
            let mut reply = if sse {
                let mut events = notice.map_or_else(String::new, |notice| {
                    format!("event: message\ndata: {notice}\n\n")
                });
                events.push_str(&format!("event: message\ndata: {message}\n\n"));
                Response::sse(events)
            } else {
                Response::json(200, message)
            };
            if method == "initialize" {
                reply = reply.header("Mcp-Session-Id", "legacy-http-session");
            } else {
                assert_eq!(request.headers["mcp-session-id"], "legacy-http-session");
                assert_eq!(request.headers["mcp-protocol-version"], "2025-11-25");
                assert!(!request.headers.contains_key("mcp-method"));
            }
            reply
        });
        let root = std::env::temp_dir().join(format!(
            "fliuno-legacy-http-binding-{}",
            uuid::Uuid::new_v4()
        ));
        fs::create_dir(&root).unwrap();
        let open = || Kernel::open(root.join("kernel.db"), Box::new(DemoProvider)).unwrap();
        let register = |kernel: &Kernel| {
            crate::connect_with_store(
                ServerConfig::Http(config(&fixture)),
                kernel.mcp_task_store().unwrap(),
                None,
            )
            .unwrap()
        };
        let mut kernel = open();
        let workspace = kernel.open_workspace(root.to_str().unwrap()).unwrap();
        let session = kernel.create_session(&workspace.id).unwrap();
        let task = kernel.start_task(&session.id, "fixture").unwrap();
        let cancellation = CancellationToken::new();
        let call = |registered: &[Box<dyn Tool>], name: &str, arguments: Value| {
            let tool = registered
                .iter()
                .find(|tool| tool.spec().name.starts_with(name))
                .unwrap();
            tool.execute(
                &ToolContext {
                    workspace: &workspace,
                    session_id: &session.id,
                    task_id: &task.id,
                },
                &ToolCall {
                    id: "legacy-call".into(),
                    name: tool.spec().name,
                    arguments,
                },
                &cancellation,
            )
            .unwrap()
        };
        let registered = register(&kernel);
        let created = call(&registered, "mcp_remote_legacy_", json!({}));
        assert_eq!(created.output["task"]["pollInterval"], 1234);
        assert!(
            created.output["task"]
                .get("ttl")
                .is_some_and(Value::is_null)
        );
        drop(registered);
        drop(kernel);
        let kernel = open();
        let registered = register(&kernel);
        let args = json!({"taskId": "legacy-job"});
        if sse {
            let observed = call(
                &registered,
                "mcp_remote_tasks_wait",
                json!({"taskId":"legacy-job","waitMs":800}),
            );
            assert_eq!(observed.output["result"]["reason"], "terminal");
            assert_eq!(observed.output["result"]["notificationReceived"], true);
            assert_eq!(observed.output["result"]["polls"], 2);
        }
        let get = call(&registered, "mcp_remote_tasks_get", args.clone());
        assert_eq!(get.output["result"]["status"], "completed");
        let result = call(&registered, "mcp_remote_tasks_result", args);
        assert!(
            result.output["result"]
                .get("structuredContent")
                .is_some_and(Value::is_null)
        );
        let requests = fixture.requests();
        assert_eq!(
            requests
                .iter()
                .filter(|r| r.body["method"] == "initialize")
                .count(),
            2
        );
        assert_eq!(
            requests
                .iter()
                .filter(|r| r.body["method"] == "tools/call")
                .count(),
            1
        );
        drop(registered);
        drop(kernel);
        for file in fs::read_dir(&root).unwrap() {
            fs::remove_file(file.unwrap().path()).unwrap();
        }
        fs::remove_dir(root).unwrap();
        assert_eq!(
            requests
                .iter()
                .filter(|r| r.body["method"] == "tasks/result")
                .count(),
            1
        );
    }
}
