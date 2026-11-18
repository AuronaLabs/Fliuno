use super::*;
use rmcp::{
    ErrorData, RoleServer, ServerHandler, ServiceExt,
    model::{
        CallToolResponse, CallToolResult, ContentBlock, ListToolsResult, PaginatedRequestParams,
        ServerCapabilities, ServerConfig,
    },
    service::RequestContext,
};
use serde_json::json;
use std::time::Duration;
use tokio::runtime::Runtime;

struct Fixture {
    delay: Duration,
}
impl ServerHandler for Fixture {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
    }
    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        Ok(serde_json::from_value(json!({"tools":[{"name":"echo","description":"Fixture echo","inputSchema":{"type":"object","required":["text"],"properties":{"text":{"type":"string"}},"additionalProperties":false},"annotations":{"readOnlyHint":true}}]})).unwrap())
    }
    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        tokio::time::sleep(self.delay).await;
        assert_eq!(request.name, "echo");
        let text = request.arguments.unwrap()["text"]
            .as_str()
            .unwrap()
            .to_owned();
        Ok(CallToolResult::success(vec![ContentBlock::text(text)]).into())
    }
}

#[test]
fn sdk_discovery_validation_and_execution_roundtrip() {
    let runtime = Runtime::new().unwrap();
    let (service, tools) = runtime.block_on(async {
        let (client_io, server_io) = tokio::io::duplex(16384);
        tokio::spawn(async {
            Fixture {
                delay: Duration::ZERO,
            }
            .serve(server_io)
            .await
            .unwrap()
            .waiting()
            .await
            .unwrap();
        });
        let service = ().serve(client_io).await.unwrap();
        let tools = service.list_all_tools().await.unwrap();
        (service, tools)
    });
    let host = Arc::new(Host {
        authority: authority_digest_for_fixture(),
        service: Mutex::new(service),
        runtime,
        timeout: TIMEOUT,
    });
    let tools = wrap_tools(host, "fixture", tools, None).unwrap();
    let tool = &tools[0];
    assert!(matches!(tool.spec().effect, ToolEffect::External));
    assert!(tool.validate(&json!({"text":123})).is_err());
    assert!(tool.validate(&json!({"text":"ok","extra":true})).is_err());
    let result = tool
        .execute(
            &ToolContext {
                session_id: "session_test",
                task_id: "task",
                workspace: &fliuno_protocol::Workspace {
                    id: "ws".into(),
                    root: "unused".into(),
                },
            },
            &ToolCall {
                id: "call".into(),
                name: tool.spec().name,
                arguments: json!({"text":"roundtrip"}),
            },
            &CancellationToken::new(),
        )
        .unwrap();
    assert_eq!(result.output["content"][0]["text"], "roundtrip");
    assert!(result.artifacts.is_empty());
}

#[test]
fn timed_out_tool_closes_the_mcp_host_and_marks_result_unknown() {
    let runtime = Runtime::new().unwrap();
    let (service, tools) = runtime.block_on(async {
        let (client_io, server_io) = tokio::io::duplex(16384);
        tokio::spawn(async {
            Fixture {
                delay: Duration::from_secs(1),
            }
            .serve(server_io)
            .await
            .unwrap()
            .waiting()
            .await
            .unwrap();
        });
        let service = ().serve(client_io).await.unwrap();
        let tools = service.list_all_tools().await.unwrap();
        (service, tools)
    });
    let host = Arc::new(Host {
        authority: authority_digest_for_fixture(),
        service: Mutex::new(service),
        runtime,
        timeout: Duration::from_millis(10),
    });
    let tools = wrap_tools(host, "fixture", tools, None).unwrap();
    let tool = &tools[0];
    let context = ToolContext {
        session_id: "session_test",
        task_id: "task",
        workspace: &fliuno_protocol::Workspace {
            id: "ws".into(),
            root: "unused".into(),
        },
    };
    let call = ToolCall {
        id: "call".into(),
        name: tool.spec().name,
        arguments: json!({"text":"timeout"}),
    };

    assert!(matches!(
        tool.execute(&context, &call, &CancellationToken::new()),
        Err(ToolError::OutcomeUnknown(message)) if message.contains("timed out")
    ));
    assert!(matches!(
        tool.execute(&context, &call, &CancellationToken::new()),
        Err(ToolError::Failed(message)) if message.contains("closed")
    ));
}

struct ControlledFixture {
    started: std::sync::mpsc::SyncSender<()>,
    release: Arc<tokio::sync::Semaphore>,
    calls: Arc<std::sync::atomic::AtomicUsize>,
}

impl ServerHandler for ControlledFixture {
    fn get_info(&self) -> ServerConfig {
        Fixture {
            delay: Duration::ZERO,
        }
        .get_info()
    }

    async fn list_tools(
        &self,
        request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        Fixture {
            delay: Duration::ZERO,
        }
        .list_tools(request, context)
        .await
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if request.arguments.as_ref().unwrap()["text"] == "first" {
            self.started.send(()).unwrap();
            self.release.acquire().await.unwrap().forget();
        }
        Fixture {
            delay: Duration::ZERO,
        }
        .call_tool(request, context)
        .await
    }
}

fn invoke(
    tool: &dyn Tool,
    text: &str,
    cancellation: &CancellationToken,
) -> Result<ToolOutput, ToolError> {
    tool.execute(
        &ToolContext {
            session_id: "session_test",
            task_id: "task",
            workspace: &fliuno_protocol::Workspace {
                id: "ws".into(),
                root: "unused".into(),
            },
        },
        &ToolCall {
            id: text.into(),
            name: tool.spec().name,
            arguments: json!({"text":text}),
        },
        cancellation,
    )
}

#[test]
fn cancelling_queued_call_returns_without_sending_or_closing_busy_host() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let runtime = Runtime::new().unwrap();
    let (started, observed) = std::sync::mpsc::sync_channel(1);
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let calls = Arc::new(AtomicUsize::new(0));
    let fixture = ControlledFixture {
        started,
        release: Arc::clone(&release),
        calls: Arc::clone(&calls),
    };
    let (service, tools) = runtime.block_on(async {
        let (client_io, server_io) = tokio::io::duplex(16384);
        tokio::spawn(async move {
            fixture
                .serve(server_io)
                .await
                .unwrap()
                .waiting()
                .await
                .unwrap();
        });
        let service = ().serve(client_io).await.unwrap();
        let tools = service.list_all_tools().await.unwrap();
        (service, tools)
    });
    let host = Arc::new(Host {
        authority: authority_digest_for_fixture(),
        service: Mutex::new(service),
        runtime,
        timeout: Duration::from_secs(5),
    });
    let mut tools = wrap_tools(host, "fixture", tools, None).unwrap();
    let tool: Arc<dyn Tool> = Arc::from(tools.remove(0));
    let first = std::thread::spawn({
        let tool = Arc::clone(&tool);
        move || invoke(tool.as_ref(), "first", &CancellationToken::new())
    });
    observed.recv_timeout(Duration::from_secs(2)).unwrap();
    let cancellation = CancellationToken::new();
    let (result_tx, result_rx) = std::sync::mpsc::channel();
    let (attempted, attempt_rx) = std::sync::mpsc::channel();
    let second = std::thread::spawn({
        let tool = Arc::clone(&tool);
        let cancellation = cancellation.clone();
        move || {
            attempted.send(()).unwrap();
            let result = invoke(tool.as_ref(), "cancelled", &cancellation);
            let _ = result_tx.send(result);
        }
    });
    attempt_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    cancellation.cancel();
    let cancelled = result_rx.recv_timeout(Duration::from_secs(1));
    let calls_before_release = calls.load(Ordering::SeqCst);
    // Always unblock and join threads, including if the regression returns.
    release.add_permits(1);
    first.join().unwrap().unwrap();
    second.join().unwrap();
    assert!(
        matches!(cancelled.unwrap(), Err(ToolError::Failed(message)) if message.contains("no request sent"))
    );
    assert_eq!(calls_before_release, 1);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        invoke(tool.as_ref(), "still_available", &CancellationToken::new())
            .unwrap()
            .output["content"][0]["text"],
        "still_available"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

#[test]
fn schema_validation_does_not_fetch_remote_references() {
    assert!(jsonschema::validator_for(&json!({"$ref":"https://example.invalid/schema"})).is_err());
}
#[test]
fn aliases_are_stable_distinct_and_provider_compatible() {
    let a = alias("abcdefghijklmnop", "a.b_非常长的工具名称");
    assert!(a.len() <= 64);
    assert!(a.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_'));
    assert_eq!(a, alias("abcdefghijklmnop", "a.b_非常长的工具名称"));
    assert_ne!(alias("s", "a.b"), alias("s", "a_b"));
}

fn authority_digest_for_fixture() -> String {
    fliuno_core::authority_digest("fixture", &json!({}))
}
