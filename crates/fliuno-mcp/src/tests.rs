use super::*;
use rmcp::{
    ErrorData, RoleServer, ServerHandler, ServiceExt,
    model::{
        CallToolResponse, CallToolResult, CancelTaskParams, ContentBlock, CreateTaskResult,
        DetailedTask, GetTaskParams, GetTaskResult, InputRequest, InputRequests, ListToolsResult,
        PaginatedRequestParams, ServerCapabilities, ServerConfig, Task, TaskPayload, TaskStatus,
        UpdateTaskParams,
    },
    service::RequestContext,
};
use serde_json::json;
use std::{
    sync::atomic::{AtomicUsize, Ordering},
    time::Duration,
};
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
        let service = client_config().serve(client_io).await.unwrap();
        let tools = service.list_all_tools().await.unwrap();
        (service, tools)
    });
    let host = Arc::new(Host {
        authority: authority_digest_for_fixture(),
        service: Mutex::new(service),
        runtime,
        timeout: TIMEOUT,
    });
    let tools = wrap_tools(host, "fixture", tools, None, false).unwrap();
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

#[derive(Clone, Default)]
struct TasksFixture {
    invalid_creation: bool,
    reads: Arc<AtomicUsize>,
    updates: Arc<std::sync::Mutex<Vec<UpdateTaskParams>>>,
    cancellations: Arc<std::sync::Mutex<Vec<CancelTaskParams>>>,
}

impl ServerHandler for TasksFixture {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_tasks()
                .build(),
        )
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        Ok(serde_json::from_value(json!({
            "tools":[{"name":"start","inputSchema":{"type":"object","additionalProperties":false}}]
        }))
        .unwrap())
    }

    async fn call_tool(
        &self,
        _request: rmcp::model::CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let task = Task::new(
            if self.invalid_creation {
                "invalid\ntask"
            } else {
                "remote-task-1"
            },
            TaskStatus::Working,
            "2026-11-19T00:00:00Z",
            "2026-11-19T00:00:00Z",
        );
        Ok(CreateTaskResult::new(task).into())
    }

    async fn get_task(
        &self,
        request: GetTaskParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<GetTaskResult, ErrorData> {
        let reads = self.reads.fetch_add(1, Ordering::Relaxed);
        let id = request.task_id.as_str();
        let payload = match id {
            "slow" | "working" => TaskPayload::Working,
            "then-input" | "then-terminal" if reads == 0 => TaskPayload::Working,
            "then-terminal" => TaskPayload::Cancelled,
            "remote-task-1" | "then-input" | "wrong-id" | "wrong-kind" => {
                let request: InputRequest =
                    serde_json::from_value(json!({"method":"roots/list","params":{}})).unwrap();
                let input_requests: InputRequests = [("roots-1".to_owned(), request)].into();
                TaskPayload::InputRequired { input_requests }
            }
            _ => return Err(ErrorData::invalid_params("unknown task", None)),
        };
        let task = Task::new(
            if id == "wrong-id" {
                "different-task"
            } else {
                id
            },
            payload.status(),
            "2026-11-19T00:00:00Z",
            "2026-11-19T00:00:01Z",
        )
        .with_poll_interval_ms(if id == "slow" { u64::MAX } else { 100 });
        let mut result = GetTaskResult::new(DetailedTask::new(task, payload));
        if id == "wrong-kind" {
            result.result_type = rmcp::model::ResultType::TASK;
        }
        Ok(result)
    }

    async fn update_task(
        &self,
        request: UpdateTaskParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<(), ErrorData> {
        self.updates.lock().unwrap().push(request);
        Ok(())
    }

    async fn cancel_task(
        &self,
        request: CancelTaskParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<(), ErrorData> {
        self.cancellations.lock().unwrap().push(request);
        Ok(())
    }
}

fn task_tools(fixture: &TasksFixture) -> Vec<Box<dyn Tool>> {
    let runtime = Runtime::new().unwrap();
    let (service, remote_tools) = runtime.block_on(async {
        let (client_io, server_io) = tokio::io::duplex(16384);
        tokio::spawn({
            let fixture = fixture.clone();
            async move {
                fixture
                    .serve(server_io)
                    .await
                    .unwrap()
                    .waiting()
                    .await
                    .unwrap();
            }
        });
        let service = client_config().serve(client_io).await.unwrap();
        let tools = service.list_all_tools().await.unwrap();
        (service, tools)
    });
    let tasks_capability = serde_json::to_value(
        &service
            .peer_info()
            .unwrap()
            .capabilities
            .extensions
            .as_ref()
            .unwrap()[rmcp::model::TASKS_EXTENSION_ID],
    )
    .unwrap();
    let host = Arc::new(Host {
        authority: authority_digest_for_fixture(),
        service: Mutex::new(service),
        runtime,
        timeout: Duration::from_secs(6),
    });
    let mut tools = wrap_tools(Arc::clone(&host), "fixture", remote_tools, None, true).unwrap();
    tools.extend(tasks::tools(Arc::clone(&host), "fixture", tasks_capability).unwrap());
    tools
}

fn task_tool<'a>(tools: &'a [Box<dyn Tool>], name: &str) -> &'a dyn Tool {
    tools
        .iter()
        .find(|tool| tool.spec().name == name)
        .unwrap()
        .as_ref()
}

#[test]
fn official_sdk_tasks_create_read_update_and_cancel_roundtrip() {
    let fixture = TasksFixture::default();
    let tools = task_tools(&fixture);
    let start = tools
        .iter()
        .find(|tool| tool.spec().name.contains("start_"))
        .unwrap();
    let created = invoke_arguments(start.as_ref(), json!({})).unwrap();
    assert_eq!(created.output["resultType"], "task");
    assert_eq!(created.output["taskId"], "remote-task-1");

    let get = tools
        .iter()
        .find(|tool| tool.spec().name == "mcp_fixture_tasks_get")
        .unwrap();
    let rejected = invoke_arguments(get.as_ref(), json!({"taskId":"missing-task"}));
    assert!(
        matches!(rejected, Err(ToolError::Failed(message)) if message.contains("rejected a read request"))
    );
    let snapshot = invoke_arguments(get.as_ref(), json!({"taskId":"remote-task-1"})).unwrap();
    assert_eq!(snapshot.output["result"]["status"], "input_required");
    assert_eq!(
        snapshot.output["result"]["inputRequests"]["roots-1"]["method"],
        "roots/list"
    );

    let update = tools
        .iter()
        .find(|tool| tool.spec().name == "mcp_fixture_tasks_update")
        .unwrap();
    let updated = invoke_arguments(
        update.as_ref(),
        json!({"taskId":"remote-task-1","inputResponses":{"roots-1":{"roots":[]}}}),
    )
    .unwrap();
    assert_eq!(updated.output["result"]["updateAccepted"], true);
    assert_eq!(
        fixture.updates.lock().unwrap()[0].input_responses["roots-1"]["roots"],
        json!([])
    );

    let cancel = tools
        .iter()
        .find(|tool| tool.spec().name == "mcp_fixture_tasks_cancel")
        .unwrap();
    let cancelled = invoke_arguments(cancel.as_ref(), json!({"taskId":"remote-task-1"})).unwrap();
    assert_eq!(cancelled.output["result"]["cancellationRequested"], true);
    assert_eq!(
        fixture.cancellations.lock().unwrap()[0].task_id,
        "remote-task-1"
    );
}

#[test]
fn official_sdk_task_wait_stops_at_actionable_states_and_poll_bounds() {
    for (id, wait_ms, reason, polls, status) in [
        ("then-input", 2_000, "input_required", 2, "input_required"),
        ("then-terminal", 2_000, "terminal", 2, "cancelled"),
        ("slow", 50, "wait_budget", 1, "working"),
        ("working", 3_000, "poll_limit", 8, "working"),
    ] {
        let fixture = TasksFixture::default();
        let tools = task_tools(&fixture);
        let wait = task_tool(&tools, "mcp_fixture_tasks_wait");
        let get = task_tool(&tools, "mcp_fixture_tasks_get");
        assert_ne!(get.authority(), wait.authority());
        assert!(wait.validate(&json!({"taskId":id,"waitMs":3001})).is_err());
        let observation = invoke_arguments(wait, json!({"taskId":id,"waitMs":wait_ms})).unwrap();
        assert_eq!(observation.output["method"], "tasks/get");
        let result = &observation.output["result"];
        assert_eq!(result["reason"], reason);
        assert_eq!(result["polls"], polls);
        assert_eq!(result["snapshot"]["status"], status);
        assert_eq!(result["snapshot"]["taskId"], id);
        assert_eq!(fixture.reads.load(Ordering::Relaxed), polls);
        assert!(fixture.updates.lock().unwrap().is_empty());
        assert!(fixture.cancellations.lock().unwrap().is_empty());
    }
}

#[test]
fn unexpected_task_id_or_result_kind_closes_connection() {
    for (method, args) in [
        ("mcp_fixture_tasks_get", json!({"taskId":"wrong-id"})),
        (
            "mcp_fixture_tasks_wait",
            json!({"taskId":"wrong-kind","waitMs":250}),
        ),
    ] {
        let fixture = TasksFixture::default();
        let tools = task_tools(&fixture);
        assert!(matches!(
            invoke_arguments(task_tool(&tools, method), args),
            Err(ToolError::OutcomeUnknown(_))
        ));
        let get = task_tool(&tools, "mcp_fixture_tasks_get");
        assert!(
            matches!(invoke_arguments(get, json!({"taskId":"remote-task-1"})), Err(ToolError::Failed(message)) if message.contains("host is closed"))
        );
        assert_eq!(fixture.reads.load(Ordering::Relaxed), 1);
    }
}

#[test]
fn invalid_created_task_handle_is_never_returned() {
    let tools = task_tools(&TasksFixture {
        invalid_creation: true,
        ..TasksFixture::default()
    });
    let start = tools
        .iter()
        .find(|tool| tool.spec().name.contains("start_"))
        .unwrap();
    assert!(matches!(
        invoke_arguments(start.as_ref(), json!({})),
        Err(ToolError::OutcomeUnknown(_))
    ));
}

#[test]
fn client_declares_tasks_without_advertising_deprecated_capabilities() {
    let config = client_config();
    assert!(config.capabilities.supports_tasks());
    assert!(config.capabilities.roots.is_none());
    assert!(config.capabilities.sampling.is_none());
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
        let service = client_config().serve(client_io).await.unwrap();
        let tools = service.list_all_tools().await.unwrap();
        (service, tools)
    });
    let host = Arc::new(Host {
        authority: authority_digest_for_fixture(),
        service: Mutex::new(service),
        runtime,
        timeout: Duration::from_millis(10),
    });
    let tools = wrap_tools(host, "fixture", tools, None, false).unwrap();
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

fn invoke_arguments(
    tool: &dyn Tool,
    arguments: serde_json::Value,
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
            id: "call".into(),
            name: tool.spec().name,
            arguments,
        },
        &CancellationToken::new(),
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
        let service = client_config().serve(client_io).await.unwrap();
        let tools = service.list_all_tools().await.unwrap();
        (service, tools)
    });
    let host = Arc::new(Host {
        authority: authority_digest_for_fixture(),
        service: Mutex::new(service),
        runtime,
        timeout: Duration::from_secs(5),
    });
    let mut tools = wrap_tools(host, "fixture", tools, None, false).unwrap();
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
