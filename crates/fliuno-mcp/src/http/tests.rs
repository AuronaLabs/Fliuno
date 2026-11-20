use super::*;
use crate::{CancellationToken, ServerConfig, Tool, ToolCall, ToolContext, ToolError};
use serde_json::json;
#[path = "../../tests/support/http.rs"]
mod wire;
use wire::{Fixture, Response, Version};

fn config(fixture: &Fixture) -> HttpServerConfig {
    serde_json::from_value(json!({"id":"remote","transport":"streamable_http","url":fixture.url("/mcp"),"timeoutMs":2000})).unwrap()
}
fn tools(config: HttpServerConfig) -> Vec<Box<dyn Tool>> {
    crate::connect_server(ServerConfig::Http(config)).unwrap()
}
fn invoke(
    tool: &dyn Tool,
    cancellation: &CancellationToken,
) -> Result<crate::ToolOutput, ToolError> {
    tool.execute(
        &ToolContext {
            session_id: "session_test",
            task_id: "fixture",
            workspace: &fliuno_protocol::Workspace {
                id: "unused".into(),
                root: "unused".into(),
            },
        },
        &ToolCall {
            id: "call".into(),
            name: tool.spec().name,
            arguments: json!({"text":"你好，远程工具"}),
        },
        cancellation,
    )
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
        let mut service = connect_with_headers(&config, validate(&config).unwrap(), resolved)
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
    for status in [307, 308, 401, 403, 500] {
        let location = target.url("/capture");
        let fixture = Fixture::start(move |_| {
            Response::json(status, json!({"private":"sensitive-fixture"}))
                .header("Location", &location)
                .header("WWW-Authenticate", "Bearer private=\"sensitive-fixture\"")
        });
        let error = crate::connect_server(ServerConfig::Http(config(&fixture)))
            .err()
            .unwrap();
        assert!(!error.contains("sensitive-fixture") && !error.contains("capture"));
        assert_eq!(fixture.requests().len(), 1);
        assert!(fixture.requests()[0].body["method"] == "server/discover");
    }
    assert!(target.requests().is_empty());
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
