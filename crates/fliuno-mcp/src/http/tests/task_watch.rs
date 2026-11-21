use super::*;

#[test]
fn modern_task_watch_checks_the_filter_and_reconciles_after_the_stream() {
    for expected in ["notified", "disconnected", "invalid_stream", "declined"] {
        let reads = std::sync::atomic::AtomicUsize::new(0);
        let fixture = Fixture::start(move |request| {
            let mut task = json!({"taskId":"remote/task-1","status":"working",
                "createdAt":"2026-11-21T00:00:00Z","lastUpdatedAt":"2026-11-21T00:00:00Z","ttlMs":null});
            let id = &request.body["id"];
            let result = match request.body["method"].as_str() {
                Some("server/discover") => {
                    json!({"resultType":"complete","supportedVersions":["2026-07-28"],
                    "capabilities":{"tools":{},"extensions":{"io.modelcontextprotocol/tasks":{}}},
                    "ttlMs":0,"cacheScope":"private"})
                }
                Some("tools/list") => {
                    json!({"tools":[{"name":"echo","inputSchema":{"type":"object"},
                    "outputSchema":{"type":"integer"}}]})
                }
                Some("tools/call") => {
                    task["resultType"] = json!("task");
                    task
                }
                Some("tasks/get") => {
                    task["resultType"] = json!("complete");
                    if reads.fetch_add(1, Ordering::Relaxed) > 0 {
                        task["status"] = json!("completed");
                        task["result"] = json!({"content":[],"structuredContent":42});
                    }
                    task
                }
                Some("subscriptions/listen") => {
                    let params = &request.body["params"];
                    assert_eq!(
                        params["notifications"],
                        json!({"taskIds":["remote/task-1"]})
                    );
                    assert_eq!(
                        params["_meta"]["io.modelcontextprotocol/protocolVersion"],
                        "2026-07-28"
                    );
                    assert_eq!(
                        params["_meta"]["io.modelcontextprotocol/clientInfo"]["name"],
                        "fliuno"
                    );
                    assert_eq!(
                        params["_meta"]["io.modelcontextprotocol/clientCapabilities"]["extensions"]
                            ["io.modelcontextprotocol/tasks"],
                        json!({})
                    );
                    assert_eq!(request.headers["mcp-method"], "subscriptions/listen");
                    assert_eq!(request.headers["mcp-protocol-version"], "2026-07-28");
                    assert!(!request.headers.contains_key("mcp-session-id"));
                    assert!(!request.headers.contains_key("last-event-id"));
                    let filter = match expected {
                        "invalid_stream" => json!({"taskIds":["remote/other-task"]}),
                        "declined" => json!({}),
                        _ => json!({"taskIds":["remote/task-1"]}),
                    };
                    let meta = json!({"io.modelcontextprotocol/subscriptionId":id});
                    let ack = json!({"jsonrpc":"2.0","method":"notifications/subscriptions/acknowledged",
                        "params":{"_meta":meta,"notifications":filter}});
                    let mut events = format!("event: message\ndata: {ack}\n\n");
                    if expected == "notified" {
                        task["_meta"] = meta;
                        task["status"] = json!("completed");
                        task["result"] =
                            json!({"content":[],"structuredContent":"unverified notice body"});
                        let updated =
                            json!({"jsonrpc":"2.0","method":"notifications/tasks","params":task});
                        events.push_str(&format!(
                            "event: message\ndata: {updated}\n\nevent: message\ndata: {updated}\n\n"
                        ));
                    }
                    return Response::sse(events);
                }
                _ => return wire::reply(request, Version::Modern),
            };
            Response::json(200, json!({"jsonrpc":"2.0","id":id,"result":result}))
        });
        let registered = tools(config(&fixture));
        let start = registered
            .iter()
            .find(|tool| tool.spec().name.starts_with("mcp_remote_echo_"))
            .unwrap();
        let watch = registered
            .iter()
            .find(|tool| tool.spec().name == "mcp_remote_tasks_watch")
            .unwrap();
        let args = json!({"taskId":"remote/task-1","waitMs":500});
        let cancel = CancellationToken::new();
        assert!(
            matches!(invoke_in_session(&**watch, "session_test", args.clone(), &cancel),
            Err(ToolError::Failed(message)) if message.contains("no local call binding"))
        );
        invoke(&**start, &cancel).unwrap();
        assert!(
            matches!(invoke_in_session(&**watch, "another", args.clone(), &cancel),
            Err(ToolError::Failed(message)) if message.contains("different workspace or session"))
        );
        let observation = invoke_in_session(&**watch, "session_test", args, &cancel)
            .unwrap()
            .output;
        assert_eq!(observation["result"]["stream"], expected);
        assert_eq!(
            observation["result"]["snapshot"]["result"]["structuredContent"],
            42
        );
        let requests = fixture.requests();
        let methods: Vec<_> = requests
            .iter()
            .filter_map(|r| r.body["method"].as_str())
            .collect();
        assert_eq!(
            methods,
            [
                "server/discover",
                "tools/list",
                "tools/call",
                "tasks/get",
                "subscriptions/listen",
                "tasks/get"
            ]
        );
    }
}
