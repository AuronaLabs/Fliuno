use super::*;
use crate::{CancellationToken, ServerConfig, Tool, ToolCall, ToolContext, ToolError};
use serde_json::{Value, json};
#[path = "../../tests/support/http.rs"]
mod wire;
use wire::{Fixture, Response, Version};

#[path = "tests/durable_tasks.rs"]
mod durable_tasks;
#[path = "tests/legacy_sse.rs"]
mod legacy_sse;
#[path = "tests/legacy_tasks.rs"]
mod legacy_tasks;
#[path = "tests/task_watch.rs"]
mod task_watch;

fn config(fixture: &Fixture) -> HttpServerConfig {
    serde_json::from_value(json!({"id":"remote","transport":"streamable_http","url":fixture.url("/mcp"),"timeoutMs":2000})).unwrap()
}

fn loopback_egress(fixture: &Fixture) -> crate::EgressPolicy {
    let port = Url::parse(&fixture.url("/mcp")).unwrap().port().unwrap();
    serde_json::from_value(
        json!({"routes":[{"host":"127.0.0.1","addresses":["127.0.0.1"],"ports":[port]}]}),
    )
    .unwrap()
}

#[tokio::test]
async fn pinned_host_uses_only_its_configured_addresses_without_dns_fallback() {
    let fixture = Fixture::start(|_| Response::text(200, "pinned"));
    let mut endpoint = Url::parse(&fixture.url("/mcp")).unwrap();
    endpoint.set_host(Some("mcp-fixture.invalid")).unwrap();
    let policy: crate::EgressPolicy = serde_json::from_value(json!({"routes":[{
        "host":"mcp-fixture.invalid","addresses":["127.0.0.1"],"ports":[endpoint.port().unwrap()]
    }]}))
    .unwrap();
    policy.validate().unwrap();
    let client = policy
        .apply(
            reqwest::Client::builder()
                .no_proxy()
                .timeout(Duration::from_secs(2)),
        )
        .build()
        .unwrap();
    assert_eq!(
        client
            .get(endpoint.clone())
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap(),
        "pinned"
    );
    // The OS can resolve localhost to the listening address; the strict
    // resolver must reject it instead of handing resolution to the OS.
    endpoint.set_host(Some("localhost")).unwrap();
    let rejected = client.get(endpoint).send().await.unwrap_err();
    assert!(rejected.is_connect());
    assert_eq!(fixture.requests().len(), 1);
}

#[test]
fn egress_rejects_unlisted_and_mismatched_literal_addresses_before_discovery() {
    let fixture = Fixture::start(|r| wire::reply(r, Version::Modern));
    for routes in [
        json!([{"host":"other.invalid","addresses":["127.0.0.1"],"ports":[80]}]),
        json!([{"host":"127.0.0.1","addresses":["127.0.0.2"],"ports":[80]}]),
        json!([{"host":"127.0.0.1","addresses":["127.0.0.1"],"ports":[80]},{"host":"127.0.0.1","addresses":["127.0.0.1"],"ports":[81]}]),
    ] {
        let mut config = config(&fixture);
        config.egress = Some(serde_json::from_value(json!({"routes":routes})).unwrap());
        assert!(
            crate::connect_server(ServerConfig::Http(config))
                .err()
                .unwrap()
                .contains("egress")
        );
    }
    assert!(fixture.requests().is_empty());
    let fixture_port = Url::parse(&fixture.url("/mcp")).unwrap().port().unwrap();
    let wrong_port = if fixture_port == u16::MAX {
        fixture_port - 1
    } else {
        fixture_port + 1
    };
    let only_other_port: crate::EgressPolicy = serde_json::from_value(json!({"routes":[
        {"host":"127.0.0.1","addresses":["127.0.0.1"],"ports":[wrong_port]}
    ]}))
    .unwrap();
    let mut wrong_port = config(&fixture);
    wrong_port.egress = Some(only_other_port);
    assert!(crate::connect_server(ServerConfig::Http(wrong_port)).is_err());
    assert!(fixture.requests().is_empty());
    let mut allowed_port = config(&fixture);
    allowed_port.egress = Some(loopback_egress(&fixture));
    let tools = tools(allowed_port);
    invoke(&*tools[0], &CancellationToken::new()).unwrap();
    assert_eq!(
        fixture
            .requests()
            .iter()
            .filter(|r| r.body["method"] == "tools/call")
            .count(),
        1
    );
}
fn tools(config: HttpServerConfig) -> Vec<Box<dyn Tool>> {
    crate::connect_server(ServerConfig::Http(config)).unwrap()
}
fn invoke(
    tool: &dyn Tool,
    cancellation: &CancellationToken,
) -> Result<crate::ToolOutput, ToolError> {
    invoke_in_session(
        tool,
        "session_test",
        json!({"text":"你好，远程工具"}),
        cancellation,
    )
}

fn invoke_in_session(
    tool: &dyn Tool,
    session_id: &str,
    arguments: Value,
    cancellation: &CancellationToken,
) -> Result<crate::ToolOutput, ToolError> {
    tool.execute(
        &ToolContext {
            session_id,
            task_id: "fixture",
            workspace: &fliuno_protocol::Workspace {
                id: "unused".into(),
                root: "unused".into(),
            },
        },
        &ToolCall {
            id: "call".into(),
            name: tool.spec().name,
            arguments,
        },
        cancellation,
    )
}

#[test]
fn modern_tasks_require_the_original_tool_contract_and_route_by_task_id() {
    for (structured_content, valid) in [(Value::Null, true), (json!("invalid private data"), false)]
    {
        let requires_input = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let remote_requires_input = Arc::clone(&requires_input);
        let reused_id = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let remote_reused_id = Arc::clone(&reused_id);
        let fixture = Fixture::start(move |request| {
            let task = json!({"taskId":"remote/task-1","status":"working",
                "createdAt":"2026-11-21T00:00:00Z","lastUpdatedAt":"2026-11-21T00:00:00Z","ttlMs":null});
            let result = match request.body["method"].as_str() {
                Some("server/discover") => {
                    json!({"resultType":"complete","supportedVersions":["2026-07-28"],
                    "capabilities":{"tools":{},"extensions":{"io.modelcontextprotocol/tasks":{}}},
                    "ttlMs":0,"cacheScope":"private"})
                }
                Some("tools/list") => {
                    json!({"tools":[{"name":"echo","inputSchema":{"type":"object"},
                    "outputSchema":{"type":"null"}}]})
                }
                Some("tools/call") => {
                    let mut task = task;
                    task["resultType"] = json!("task");
                    task
                }
                Some("tasks/get") => {
                    let mut task = task;
                    task["resultType"] = json!("complete");
                    if remote_reused_id.load(Ordering::Relaxed) {
                        task["createdAt"] = json!("2026-11-21T00:01:00Z");
                    }
                    if remote_requires_input.load(Ordering::Relaxed) {
                        task["status"] = json!("input_required");
                        task["inputRequests"] = json!({"form-1":{"method":"elicitation/create",
                            "params":{"mode":"form","message":"Continue?",
                                "requestedSchema":{"type":"object","properties":{}}}}});
                    } else {
                        task["status"] = json!("completed");
                        task["result"] =
                            json!({"content": [], "structuredContent": structured_content});
                    }
                    task
                }
                Some("tasks/cancel" | "tasks/update") => json!({"resultType":"complete"}),
                _ => return wire::reply(request, Version::Modern),
            };
            Response::json(
                200,
                json!({"jsonrpc":"2.0","id":request.body["id"],"result":result}),
            )
        });
        let registered = tools(config(&fixture));
        let select = |name| {
            registered
                .iter()
                .find(|tool| tool.spec().name == name)
                .unwrap()
                .as_ref()
        };
        let cancel = CancellationToken::new();
        let start = registered
            .iter()
            .find(|tool| tool.spec().name.starts_with("mcp_remote_echo_"))
            .unwrap();
        assert_eq!(
            invoke(&**start, &cancel).unwrap().output["taskId"],
            "remote/task-1"
        );
        let get = select("mcp_remote_tasks_get");
        let args = json!({"taskId":"remote/task-1"});
        for tool in [get, select("mcp_remote_tasks_cancel")] {
            assert!(matches!(
                invoke_in_session(tool, "another_session", args.clone(), &cancel),
                Err(ToolError::Failed(message)) if message.contains("different workspace or session")
            ));
        }
        assert!(!fixture.requests().iter().any(|r| {
            r.body["method"]
                .as_str()
                .is_some_and(|m| m.starts_with("tasks/"))
        }));
        for (tool, args, pointer) in [
            (get, args.clone(), "/result/result/structuredContent"),
            (
                select("mcp_remote_tasks_wait"),
                json!({"taskId":"remote/task-1","waitMs":100}),
                "/result/snapshot/result/structuredContent",
            ),
        ] {
            match invoke_in_session(tool, "session_test", args, &cancel) {
                Ok(result) if valid => {
                    assert!(result.output.pointer(pointer).is_some_and(Value::is_null))
                }
                Err(ToolError::OutcomeUnknownWithOutput { message, output }) if !valid => {
                    assert!(
                        message.contains("original tool schema")
                            && !message.contains("invalid private data")
                    );
                    assert_eq!(
                        output,
                        json!({"taskId":"remote/task-1","status":"completed"})
                    );
                }
                outcome => panic!("unexpected Tasks output validation: {}", outcome.is_ok()),
            }
        }
        let fresh = tools(config(&fixture));
        let fresh_get = fresh
            .iter()
            .find(|tool| tool.spec().name == "mcp_remote_tasks_get")
            .unwrap();
        assert!(
            matches!(invoke_in_session(&**fresh_get, "session_test", args.clone(), &cancel),
            Err(ToolError::OutcomeUnknownWithOutput { message, .. }) if message.contains("no local call binding"))
        );
        requires_input.store(true, Ordering::Relaxed);
        let unbound =
            invoke_in_session(&**fresh_get, "session_test", args.clone(), &cancel).unwrap();
        assert_eq!(
            unbound.output["result"],
            json!({
                "taskId":"remote/task-1", "status":"input_required", "unbound":true
            })
        );
        let updated = invoke_in_session(
            select("mcp_remote_tasks_update"),
            "session_test",
            json!({"taskId":"remote/task-1","inputResponses":{"form-1":{"action":"decline"}}}),
            &cancel,
        )
        .unwrap();
        assert_eq!(updated.output["result"]["updateAccepted"], true);
        let cancel_tool = select("mcp_remote_tasks_cancel");
        reused_id.store(true, Ordering::Relaxed);
        assert!(matches!(
            invoke_in_session(cancel_tool, "session_test", args.clone(), &cancel),
            Err(ToolError::Failed(message)) if message.contains("original handle")
        ));
        reused_id.store(false, Ordering::Relaxed);
        let cancelled = invoke_in_session(cancel_tool, "session_test", args, &cancel).unwrap();
        assert_eq!(cancelled.output["result"]["cancellationRequested"], true);
        let requests = fixture.requests();
        assert_eq!(
            requests
                .iter()
                .filter(|r| r.body["method"] == "tools/call")
                .count(),
            1
        );
        let task_requests: Vec<_> = requests
            .iter()
            .filter(|r| {
                r.body["method"]
                    .as_str()
                    .is_some_and(|m| m.starts_with("tasks/"))
            })
            .collect();
        let methods: Vec<_> = task_requests
            .iter()
            .map(|r| r.body["method"].as_str().unwrap())
            .collect();
        assert_eq!(
            methods,
            [
                "tasks/get",
                "tasks/get",
                "tasks/get",
                "tasks/get",
                "tasks/get",
                "tasks/update",
                "tasks/get",
                "tasks/get",
                "tasks/cancel"
            ]
        );
        for request in task_requests {
            assert_eq!(request.headers["mcp-method"], request.body["method"]);
            assert_eq!(request.headers["mcp-name"], "remote/task-1");
            assert_eq!(request.body["params"]["taskId"], "remote/task-1");
            assert!(request.body["params"]["_meta"]["io.modelcontextprotocol/clientCapabilities"]["extensions"]
                .get("io.modelcontextprotocol/tasks").is_some());
        }
    }
}

#[test]
fn modern_tool_schema_validates_before_dispatch_and_after_complete_reply() {
    let fixture = Fixture::start(|request| {
        let id = &request.body["id"];
        match request.body["method"].as_str() {
            Some("tools/list") => Response::json(
                200,
                json!({"jsonrpc":"2.0","id":id,"result":{"tools":[{
                    "name":"echo",
                    "inputSchema":{"type":"object","required":["text"],
                        "$defs":{"text":{"anyOf":[{"const":"你好，远程工具"},{"const":"short"}]}},
                        "properties":{"text":{"$ref":"#/$defs/text"}},
                        "if":{"properties":{"text":{"const":"short"}}},
                        "then":{"required":["flag"]}},
                    "outputSchema":{"type":"array","prefixItems":[{"type":"integer"},{"type":"string"}],
                        "items":false,"minItems":2}
                }]}}),
            ),
            Some("tools/call") => Response::json(
                200,
                json!({"jsonrpc":"2.0","id":id,
                "result":{"resultType":"complete","structuredContent":[42,"answer"]}}),
            ),
            _ => wire::reply(request, Version::Modern),
        }
    });
    let tools = tools(config(&fixture));
    assert!(tools[0].validate(&json!({"text":"short"})).is_err());
    assert!(
        tools[0]
            .validate(&json!({"text":"short","flag":true}))
            .is_ok()
    );
    assert!(
        tools[0]
            .validate(&json!({"text":"unexpected","flag":true}))
            .is_err()
    );
    assert!(
        !fixture
            .requests()
            .iter()
            .any(|r| r.body["method"] == "tools/call")
    );
    let output = invoke(&*tools[0], &CancellationToken::new())
        .unwrap()
        .output;
    assert_eq!(output["structuredContent"], json!([42, "answer"]));
}

#[test]
fn output_contract_distinguishes_null_missing_invalid_and_tool_errors() {
    for (result, valid) in [
        (json!({"structuredContent":null}), true),
        (json!({"content":[]}), false),
        (json!({"structuredContent":"private-invalid-output"}), false),
        (
            json!({"isError":true,"content":[{"type":"text","text":"tool error"}]}),
            true,
        ),
    ] {
        let fixture = Fixture::start(move |request| {
            let id = &request.body["id"];
            match request.body["method"].as_str() {
                Some("tools/list") => Response::json(
                    200,
                    json!({"jsonrpc":"2.0","id":id,"result":{"tools":[{
                        "name":"echo","inputSchema":{"type":"object"},"outputSchema":{"type":"null"}
                    }]}}),
                ),
                Some("tools/call") => {
                    Response::json(200, json!({"jsonrpc":"2.0","id":id,"result":result}))
                }
                _ => wire::reply(request, Version::Modern),
            }
        });
        let tools = tools(config(&fixture));
        match invoke(&*tools[0], &CancellationToken::new()) {
            Ok(output) if valid => {
                assert!(
                    output
                        .output
                        .get("structuredContent")
                        .is_some_and(Value::is_null)
                        || output.output["isError"] == true
                );
            }
            Err(ToolError::OutcomeUnknown(error)) if !valid => {
                assert!(!error.contains("private-invalid-output"));
            }
            _ => panic!("unexpected structured output validation result"),
        }
        assert_eq!(
            fixture
                .requests()
                .iter()
                .filter(|r| r.body["method"] == "tools/call")
                .count(),
            1
        );
    }
}

#[test]
fn legacy_tool_continues_to_accept_unstructured_output() {
    let fixture = Fixture::start(|request| {
        if request.body["method"] == "tools/list" {
            return Response::json(
                200,
                json!({"jsonrpc":"2.0","id":request.body["id"],"result":{"tools":[{
                    "name":"echo","inputSchema":{"properties":{"text":{"type":"string"}}},
                    "outputSchema":{"type":"object"}
                }]}}),
            );
        }
        wire::reply(request, Version::Legacy("2025-11-25"))
    });
    assert!(invoke(&*tools(config(&fixture))[0], &CancellationToken::new()).is_ok());
}

#[test]
fn modern_discovery_and_sse_call_use_self_contained_requests_without_initialize() {
    let fixture = Fixture::start(|request| {
        if request.body["method"] == "tools/call" {
            return Response::sse(format!(
                "event: message\ndata: {}\n\n",
                json!({"jsonrpc":"2.0","id":request.body["id"],"result":{"content":[{"type":"text","text":"你好，远程工具"}]}})
            ));
        }
        wire::reply(request, Version::Modern)
    });
    let tools = tools(config(&fixture));
    assert!(tools[0].validate(&json!({"text":4})).is_err());
    let result = invoke(&*tools[0], &CancellationToken::new()).unwrap();
    assert_eq!(result.output["content"][0]["text"], "你好，远程工具");
    drop(tools);
    let requests = fixture.requests();
    assert_eq!(
        requests
            .iter()
            .filter(|r| r.body["method"] == "tools/call")
            .count(),
        1
    );
    assert!(!requests.iter().any(|r| matches!(
        r.body["method"].as_str(),
        Some("initialize" | "notifications/initialized")
    )));
    for request in requests.iter().filter(|r| r.method == "POST") {
        assert_eq!(request.path, "/mcp");
        assert_eq!(request.headers["mcp-method"], request.body["method"]);
        assert_eq!(request.headers["mcp-protocol-version"], "2026-07-28");
        assert!(!request.headers.contains_key("mcp-session-id"));
        assert!(request.headers["accept"].contains("application/json"));
        assert!(request.headers["accept"].contains("text/event-stream"));
        assert_eq!(
            request.body["params"]["_meta"]["io.modelcontextprotocol/protocolVersion"],
            "2026-07-28"
        );
        assert!(
            request.body["params"]["_meta"]["io.modelcontextprotocol/clientCapabilities"]
                .is_object()
        );
    }
    let call = requests
        .iter()
        .find(|r| r.body["method"] == "tools/call")
        .unwrap();
    assert_eq!(call.headers["mcp-name"], "echo");
}

#[test]
fn resource_cache_obeys_scope_uri_ttl_and_connection_boundary() {
    for scope in ["public", "private"] {
        let fixture = Fixture::start(move |request| {
            let id = &request.body["id"];
            match request.body["method"].as_str() {
                Some("server/discover") => Response::json(
                    200,
                    json!({"jsonrpc":"2.0","id":id,
                    "result":{"resultType":"complete","supportedVersions":["2026-07-28"],
                        "ttlMs":0,"cacheScope":"private","capabilities":{"resources":{}}}}),
                ),
                Some("resources/read") => {
                    let uri = &request.body["params"]["uri"];
                    let ttl = if uri == "fixture:uncached" { 0 } else { 60_000 };
                    assert_eq!(request.headers["mcp-method"], "resources/read");
                    assert_eq!(request.headers["mcp-name"], uri.as_str().unwrap());
                    Response::json(
                        200,
                        json!({"jsonrpc":"2.0","id":id,"result":{
                        "contents":[{"uri":uri,"text":"fixture"}],"ttlMs":ttl,"cacheScope":scope}}),
                    )
                }
                _ => panic!("unexpected wire request"),
            }
        });
        let read = |registration: &[Box<dyn Tool>], uri| {
            let tool = registration
                .iter()
                .find(|tool| tool.spec().name == "mcp_remote_resources_read")
                .unwrap();
            tool.execute(
                &ToolContext {
                    session_id: "test",
                    task_id: "test",
                    workspace: &fliuno_protocol::Workspace {
                        id: "unused".into(),
                        root: "unused".into(),
                    },
                },
                &ToolCall {
                    id: "test".into(),
                    name: tool.spec().name,
                    arguments: json!({"uri":uri}),
                },
                &CancellationToken::new(),
            )
            .unwrap()
        };
        let registration = tools(config(&fixture));
        for uri in [
            "fixture:cached",
            "fixture:cached",
            "fixture:uncached",
            "fixture:uncached",
        ] {
            let output = read(&registration, uri).output;
            assert_eq!(output["result"]["contents"][0]["uri"], uri);
            assert_eq!(output["grantsPermissions"], false);
        }
        let replacement = tools(config(&fixture));
        read(&replacement, "fixture:cached");
        assert_eq!(
            fixture
                .requests()
                .iter()
                .filter(|r| r.body["method"] == "resources/read")
                .count(),
            4
        );
    }
}

#[test]
fn annotated_tool_argument_uses_the_standard_base64_parameter_header() {
    let fixture = Fixture::start(|request| {
        if request.body["method"] == "tools/list" {
            return Response::json(
                200,
                json!({
                    "jsonrpc":"2.0",
                    "id":request.body["id"],
                    "result":{"tools":[{"name":"echo","inputSchema":{"type":"object","properties":{"text":{"type":"string","x-mcp-header":"x-query"}},"required":["text"],"additionalProperties":false}}]}
                }),
            );
        }
        wire::reply(request, Version::Modern)
    });
    let tools = tools(config(&fixture));
    invoke(&*tools[0], &CancellationToken::new()).unwrap();
    drop(tools);

    let call = fixture
        .requests()
        .into_iter()
        .find(|r| r.body["method"] == "tools/call")
        .unwrap();
    let encoded = call.headers["mcp-param-x-query"]
        .strip_prefix("=?base64?")
        .unwrap()
        .strip_suffix("?=")
        .unwrap();
    assert_eq!(encoded, "5L2g5aW977yM6L+c56iL5bel5YW3");
}

#[test]
fn auto_falls_back_to_legacy_and_carries_session_and_selected_version() {
    for version in ["2025-03-26", "2025-06-18", "2025-11-25"] {
        let fixture = Fixture::start(move |r| wire::reply(r, Version::Legacy(version)));
        let tools = tools(config(&fixture));
        assert_eq!(
            invoke(&*tools[0], &CancellationToken::new())
                .unwrap()
                .output["content"][0]["text"],
            "你好，远程工具"
        );
        drop(tools);
        let requests = fixture.requests();
        assert!(
            requests
                .iter()
                .any(|r| r.body["method"] == "server/discover")
        );
        assert_eq!(
            requests
                .iter()
                .filter(|r| r.body["method"] == "initialize")
                .count(),
            1
        );
        let initialize = requests
            .iter()
            .find(|r| r.body["method"] == "initialize")
            .unwrap();
        assert_eq!(initialize.body["params"]["protocolVersion"], "2025-11-25");
        for request in requests
            .iter()
            .filter(|r| r.body["method"] == "tools/list" || r.body["method"] == "tools/call")
        {
            assert_eq!(request.headers["mcp-session-id"], "fixture-session");
            assert_eq!(request.headers["mcp-protocol-version"], version);
        }
        assert!(
            requests
                .iter()
                .any(|r| r.method == "DELETE" && r.headers["mcp-session-id"] == "fixture-session")
        );
    }
}

#[test]
fn explicit_lifecycle_modes_do_not_silently_change_the_wire_protocol() {
    let fixture = Fixture::start(|r| wire::reply(r, Version::Legacy("2025-11-25")));
    let mut legacy = config(&fixture);
    legacy.lifecycle = crate::Lifecycle::Legacy;
    drop(tools(legacy));
    assert!(
        !fixture
            .requests()
            .iter()
            .any(|r| r.body["method"] == "server/discover")
    );
    let fixture = Fixture::start(|r| wire::reply(r, Version::Legacy("2025-11-25")));
    let mut modern = config(&fixture);
    modern.lifecycle = crate::Lifecycle::Discover;
    assert!(crate::connect_server(ServerConfig::Http(modern)).is_err());
    assert!(
        !fixture
            .requests()
            .iter()
            .any(|r| r.body["method"] == "initialize")
    );
}

#[test]
fn advertised_version_fallback_requires_auto_mode_and_a_known_http_legacy_version() {
    for (version, lifecycle, should_connect) in [
        ("2025-11-25", crate::Lifecycle::Auto, true),
        ("2025-11-25", crate::Lifecycle::Discover, false),
        ("2024-11-05", crate::Lifecycle::Auto, false),
        ("2025-12-01", crate::Lifecycle::Auto, false),
        ("2099-01-01", crate::Lifecycle::Auto, false),
    ] {
        let fixture = Fixture::start(move |request| {
            if request.body["method"] == "server/discover" {
                return Response::json(
                    200,
                    json!({"jsonrpc":"2.0","id":request.body["id"],"result":{"resultType":"complete","supportedVersions":[version],"capabilities":{"tools":{}},"ttlMs":0,"cacheScope":"private"}}),
                );
            }
            wire::reply(request, Version::Legacy("2025-11-25"))
        });
        let mut config = config(&fixture);
        config.lifecycle = lifecycle;
        let result = crate::connect_server(ServerConfig::Http(config));
        assert_eq!(result.is_ok(), should_connect, "{version} / {lifecycle:?}");
        drop(result);
        assert_eq!(
            fixture
                .requests()
                .iter()
                .filter(|r| r.body["method"] == "initialize")
                .count(),
            usize::from(should_connect),
        );
    }
}

#[test]
fn http_discovery_classifies_400_bodies_before_falling_back() {
    for (code, supported, expected_discoveries, connects) in [
        (None, json!(null), 1, true),
        (Some(-32601), json!(null), 1, true),
        (Some(-32022), json!(["2025-11-25"]), 1, true),
        (Some(-32022), json!(["2025-03-26", "2025-06-18"]), 1, true),
        (Some(-32022), json!(["2026-07-28", "2025-11-25"]), 2, false),
        (Some(-32022), json!(["2099-01-01"]), 1, false),
        (Some(-32021), json!(null), 1, false),
        (Some(-32020), json!(null), 1, false),
        (Some(-32603), json!(null), 1, false),
    ] {
        let legacy = if supported
            .as_array()
            .is_some_and(|versions| versions.contains(&json!("2025-06-18")))
        {
            "2025-06-18"
        } else {
            "2025-11-25"
        };
        let fixture = Fixture::start(move |request| {
            if request.body["method"] != "server/discover" {
                if request.body["method"] == "initialize" {
                    assert_eq!(request.body["params"]["protocolVersion"], legacy);
                }
                return wire::reply(request, Version::Legacy(legacy));
            }
            match code {
                Some(code) => Response::json(
                    400,
                    json!({
                        "jsonrpc":"2.0","id":"middleware-error",
                        "error":{"code":code,"message":"private diagnostic","data":{"supported":supported}}
                    }),
                ),
                None => Response::text(400, "unrecognized protocol version"),
            }
        });
        let result = crate::connect_server(ServerConfig::Http(config(&fixture)));
        assert_eq!(result.is_ok(), connects, "HTTP 400 with {code:?}");
        if let Err(ref error) = result {
            assert!(!error.contains("private diagnostic"));
        }
        drop(result);
        let requests = fixture.requests();
        assert_eq!(
            requests
                .iter()
                .filter(|r| r.body["method"] == "server/discover")
                .count(),
            expected_discoveries,
        );
        assert_eq!(
            requests
                .iter()
                .filter(|r| r.body["method"] == "initialize")
                .count(),
            usize::from(connects),
        );
    }
}

#[test]
fn discovery_eof_does_not_attempt_initialize() {
    let fixture = Fixture::start(|_| Response::disconnect());
    assert!(crate::connect_server(ServerConfig::Http(config(&fixture))).is_err());
    let requests = fixture.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].body["method"], "server/discover");
}

#[test]
fn http_auto_keeps_waiting_for_discovery_within_the_host_budget() {
    let fixture = Fixture::start(|request| {
        if request.body["method"] == "server/discover" {
            // The SDK's Auto timer used to switch to initialize at 10 seconds.
            std::thread::sleep(Duration::from_millis(10_300));
            return wire::reply(request, Version::Modern);
        }
        if request.body["method"] == "initialize" {
            return Response::text(400, "modern server");
        }
        wire::reply(request, Version::Modern)
    });
    let mut config = config(&fixture);
    config.timeout_ms = 16_000;
    drop(tools(config));
    assert!(
        !fixture
            .requests()
            .iter()
            .any(|r| r.body["method"] == "initialize")
    );
}

#[test]
fn connection_failure_or_duplicate_alias_never_partially_registers_tools() {
    for duplicate_alias in [false, true] {
        let fixture = Fixture::start(move |request| {
            if request.path == "/failing" {
                if !duplicate_alias {
                    return Response::json(500, json!({"private":"fixture"}));
                }
                if request.body["method"] == "tools/list" {
                    let tool = json!({"name":"echo","inputSchema":{"type":"object"}});
                    return Response::json(
                        200,
                        json!({"jsonrpc":"2.0","id":request.body["id"],"result":{"tools":[tool,tool]}}),
                    );
                }
            }
            wire::reply(request, Version::Modern)
        });
        let config = serde_json::from_value(json!({"servers":[
            {"id":"first","transport":"streamable_http","url":fixture.url("/mcp")},
            {"id":"second","transport":"streamable_http","url":fixture.url("/failing")}
        ]}))
        .unwrap();
        let mut kernel =
            fliuno_core::Kernel::in_memory(Box::new(fliuno_core::DemoProvider)).unwrap();
        let original = kernel.tool_specs();
        assert!(crate::register_config(&mut kernel, config).is_err());
        assert_eq!(
            kernel
                .tool_specs()
                .iter()
                .map(|tool| &tool.name)
                .collect::<Vec<_>>(),
            original.iter().map(|tool| &tool.name).collect::<Vec<_>>(),
        );
        assert!(
            fixture
                .requests()
                .iter()
                .any(|r| r.path == "/mcp" && r.body["method"] == "tools/list")
        );
        assert!(
            !fixture
                .requests()
                .iter()
                .any(|r| r.body["method"] == "tools/call")
        );
    }
}

#[test]
fn credentials_are_resolved_explicitly_and_all_header_values_are_sensitive() {
    let fixture = Fixture::start(|r| wire::reply(r, Version::Modern));
    let mut config = config(&fixture);
    config.bearer_token_env = Some("TEST_TOKEN".into());
    config
        .headers_env
        .insert("X-Vendor-Key".into(), "TEST_VENDOR".into());
    validate(&config).unwrap();
    assert!(headers(&config, |_| None).unwrap_err().contains("missing"));
    let resolved = headers(&config, |name| match name {
        "TEST_TOKEN" => Some("fixture.token".into()),
        "TEST_VENDOR" => Some("vendor-fixture".into()),
        _ => None,
    })
    .unwrap();
    assert_eq!(resolved[AUTHORIZATION], "Bearer fixture.token");
    assert!(resolved[AUTHORIZATION].is_sensitive() && resolved["x-vendor-key"].is_sensitive());
    let runtime = tokio::runtime::Runtime::new().unwrap();
    runtime.block_on(async {
        let (mut service, _, _) =
            connect_with_headers(&config, validate(&config).unwrap(), resolved, None)
                .await
                .unwrap();
        crate::discovery::tools(&service).await.unwrap();
        service.close().await.unwrap();
    });
    for request in fixture.requests() {
        assert_eq!(request.headers["authorization"], "Bearer fixture.token");
        assert_eq!(request.headers["x-vendor-key"], "vendor-fixture");
    }
    assert!(
        !format!(
            "{:?}",
            headers(&config, |_| Some("bad\r\nprivate-value".into())).unwrap_err()
        )
        .contains("private-value")
    );
}

#[test]
fn expired_session_never_reinitializes_or_replays_an_uncertain_tool_call() {
    let fixture = Fixture::start(|r| {
        if r.body["method"] == "tools/call" {
            Response::json(404, json!({"private":"do-not-record"}))
        } else {
            wire::reply(r, Version::Legacy("2025-11-25"))
        }
    });
    let tools = tools(config(&fixture));
    let Err(ToolError::OutcomeUnknown(error)) = invoke(&*tools[0], &CancellationToken::new())
    else {
        panic!("expired call was not unknown");
    };
    assert!(!error.contains("do-not-record"));
    assert!(matches!(
        invoke(&*tools[0], &CancellationToken::new()),
        Err(ToolError::Failed(_))
    ));
    drop(tools);
    let requests = fixture.requests();
    assert_eq!(
        requests
            .iter()
            .filter(|r| r.body["method"] == "initialize")
            .count(),
        1
    );
    assert_eq!(
        requests
            .iter()
            .filter(|r| r.body["method"] == "tools/call")
            .count(),
        1
    );
}

#[test]
fn redirects_and_authentication_errors_are_redacted_without_followup_requests() {
    let target = Fixture::start(|r| wire::reply(r, Version::Modern));
    for (status, challenge) in [
        (307, false),
        (308, false),
        (401, true),
        (401, false),
        (403, true),
        (403, false),
        (405, false),
        (408, false),
        (429, false),
        (500, false),
        (503, false),
    ] {
        let location = target.url("/capture");
        let fixture = Fixture::start(move |request| {
            let response = Response::json(
                status,
                json!({"jsonrpc":"2.0","id":request.body["id"],
                "error":{"code":-32601,"message":"sensitive-fixture"}}),
            )
            .header("Location", &location);
            if challenge {
                response.header("WWW-Authenticate", "Bearer private=\"sensitive-fixture\"")
            } else {
                response
            }
        });
        let mut config = config(&fixture);
        config.egress = Some(loopback_egress(&fixture));
        let error = crate::connect_server(ServerConfig::Http(config))
            .err()
            .unwrap();
        assert!(!error.contains("sensitive-fixture") && !error.contains("capture"));
        assert_eq!(fixture.requests().len(), 1);
        assert!(fixture.requests()[0].body["method"] == "server/discover");
    }
    assert!(target.requests().is_empty());
}

#[test]
fn bare_discovery_404_does_not_attempt_initialize() {
    let fixture = Fixture::start(|_| Response::text(404, "endpoint not found"));
    assert!(crate::connect_server(ServerConfig::Http(config(&fixture))).is_err());
    assert_eq!(fixture.requests().len(), 1);
}

#[test]
fn oversized_json_tool_response_is_bounded_before_decoding_and_not_replayed() {
    let fixture = Fixture::start(|request| {
        if request.body["method"] == "tools/call" {
            let text = "x".repeat(1024 * 1024);
            return Response::json(
                200,
                json!({"jsonrpc":"2.0","id":request.body["id"],"result":{"content":[{"type":"text","text":text}]}}),
            );
        }
        wire::reply(request, Version::Modern)
    });
    let tools = tools(config(&fixture));
    assert!(matches!(
        invoke(&*tools[0], &CancellationToken::new()),
        Err(ToolError::OutcomeUnknown(_))
    ));
    drop(tools);
    assert_eq!(
        fixture
            .requests()
            .iter()
            .filter(|r| r.body["method"] == "tools/call")
            .count(),
        1
    );
}

#[test]
fn cancellation_and_timeout_stop_http_calls_and_close_the_shared_host() {
    for cancel in [true, false] {
        let fixture = Fixture::start(|r| {
            if r.body["method"] == "tools/call" {
                Response::hold()
            } else {
                wire::reply(r, Version::Modern)
            }
        });
        let mut config = config(&fixture);
        config.timeout_ms = 400;
        let tools = tools(config);
        let cancellation = CancellationToken::new();
        std::thread::scope(|scope| {
            let call = scope.spawn(|| invoke(&*tools[0], &cancellation));
            fixture.wait_for("tools/call");
            if cancel {
                cancellation.cancel();
            }
            assert!(matches!(
                call.join().unwrap(),
                Err(ToolError::OutcomeUnknown(_))
            ));
        });
        assert!(matches!(
            invoke(&*tools[0], &CancellationToken::new()),
            Err(ToolError::Failed(_))
        ));
        assert_eq!(
            fixture
                .requests()
                .iter()
                .filter(|r| r.body["method"] == "tools/call")
                .count(),
            1
        );
    }
}

#[test]
fn oversized_sse_events_fail_closed_and_tool_errors_remain_known_results() {
    for oversized in [true, false] {
        let fixture = Fixture::start(move |r| {
            if r.body["method"] != "tools/call" {
                return wire::reply(r, Version::Modern);
            }
            if oversized {
                Response::sse(format!("data: {}", "x".repeat(1024 * 1024 + 1)))
            } else {
                Response::json(
                    200,
                    json!({"jsonrpc":"2.0","id":r.body["id"],"result":{"content":[{"type":"text","text":"known failure"}],"isError":true}}),
                )
            }
        });
        let tools = tools(config(&fixture));
        let result = invoke(&*tools[0], &CancellationToken::new());
        if oversized {
            assert!(matches!(result, Err(ToolError::OutcomeUnknown(_))));
        } else {
            assert_eq!(result.unwrap().output["isError"], true);
        }
    }
}

#[test]
fn multi_round_results_are_returned_without_an_implicit_sdk_retry() {
    for result in [
        json!({"resultType":"input_required","requestState":"opaque-private-state"}),
        json!({"resultType":"input_required","content":[],"requestState":"opaque-private-state"}),
        json!({"resultType":"task","content":[]}),
        json!({"resultType":"extension","content":[]}),
    ] {
        let input_required = result["resultType"] == "input_required";
        let fixture = Fixture::start(move |r| {
            if r.body["method"] != "tools/call" {
                return wire::reply(r, Version::Modern);
            }
            Response::json(
                200,
                json!({"jsonrpc":"2.0","id":r.body["id"],"result":result}),
            )
        });
        let tools = tools(config(&fixture));
        let response = invoke(&*tools[0], &CancellationToken::new());
        if input_required {
            assert!(matches!(
                response,
                Err(ToolError::InputRequired { request_state: Some(state), .. })
                    if state == "opaque-private-state"
            ));
        } else {
            let Err(ToolError::OutcomeUnknown(message)) = response else {
                panic!("unsupported non-final result was accepted");
            };
            assert!(
                message.contains("result unknown") && !message.contains("opaque-private-state")
            );
        }
        assert_eq!(
            fixture
                .requests()
                .iter()
                .filter(|r| r.body["method"] == "tools/call")
                .count(),
            1
        );
        if !input_required {
            assert!(matches!(
                invoke(&*tools[0], &CancellationToken::new()),
                Err(ToolError::Failed(_))
            ));
        }
    }
}

#[test]
fn discovery_rejects_repeated_cursors_page_overflow_and_excessive_tool_counts() {
    for case in ["repeated", "pages", "tools", "result_type"] {
        let fixture = Fixture::start(move |r| {
            if r.body["method"] != "tools/list" {
                return wire::reply(r, Version::Modern);
            }
            let result = match case {
                "repeated" => json!({"tools":[],"nextCursor":"same"}),
                "result_type" => json!({"tools":[],"resultType":"input_required"}),
                "pages" => {
                    let n = r.body["params"]["cursor"]
                        .as_str()
                        .unwrap_or("0")
                        .parse::<u32>()
                        .unwrap();
                    json!({"tools":[],"nextCursor":(n+1).to_string()})
                }
                _ => {
                    json!({"tools":vec![json!({"name":"echo","inputSchema":{"type":"object"}});257]})
                }
            };
            Response::json(
                200,
                json!({"jsonrpc":"2.0","id":r.body["id"],"result":result}),
            )
        });
        assert!(crate::connect_server(ServerConfig::Http(config(&fixture))).is_err());
        let expected = match case {
            "repeated" => 2,
            "pages" => 32,
            _ => 1,
        };
        assert_eq!(
            fixture
                .requests()
                .iter()
                .filter(|r| r.body["method"] == "tools/list")
                .count(),
            expected
        );
    }
}

#[test]
fn configuration_rejects_mixed_transports_reserved_headers_and_unprotected_endpoints() {
    let fixture = Fixture::start(|r| wire::reply(r, Version::Modern));
    for url in [
        "http://example.test/mcp",
        "file:///tmp/mcp",
        "https://user:private@example.test/mcp",
        "https://example.test/mcp#fragment",
        "http://127.0.0.1/mcp\r\n",
    ] {
        let mut config = config(&fixture);
        config.url = url.into();
        assert!(validate(&config).is_err());
    }
    for name in [
        "Host",
        "Mcp-Protocol-Version",
        "MCP-Param-Foo",
        "Accept",
        "Last-Event-Id",
        "Content-Length",
        "Proxy-Authorization",
    ] {
        let mut config = config(&fixture);
        config.headers_env.insert(name.into(), "VALUE".into());
        assert!(validate(&config).is_err());
    }
    let mut config = config(&fixture);
    config.bearer_token_env = Some("TOKEN".into());
    config
        .headers_env
        .insert("Authorization".into(), "OTHER".into());
    assert!(validate(&config).is_err());
    for value in [
        json!({"id":"x","url":fixture.url("/mcp")}),
        json!({"id":"x","transport":"streamable_http","url":fixture.url("/mcp"),"command":"unused"}),
        json!({"id":"x","transport":"sse","url":fixture.url("/mcp")}),
        json!({"id":"x","transport":"streamable_http","url":fixture.url("/mcp"),"token":"literal-secret"}),
    ] {
        assert!(serde_json::from_value::<ServerConfig>(value).is_err());
    }
    assert!(fixture.requests().is_empty());

    let mut kernel = fliuno_core::Kernel::in_memory(Box::new(fliuno_core::DemoProvider)).unwrap();
    let duplicate = serde_json::from_value(json!({"servers":[
        {"id":"same","transport":"streamable_http","url":fixture.url("/mcp")},
        {"id":"same","transport":"streamable_http","url":fixture.url("/mcp")}
    ]}))
    .unwrap();
    assert!(
        crate::register_config(&mut kernel, duplicate)
            .unwrap_err()
            .contains("duplicate")
    );
    assert!(fixture.requests().is_empty());
}
