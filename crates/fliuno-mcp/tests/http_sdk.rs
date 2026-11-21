//! Both sides use the official SDK; raw wire tests separately check rejection
//! and replay boundaries. This test verifies JSON/SSE and lifecycle interop.
use fliuno_core::{CancellationToken, Tool, ToolContext};
use fliuno_protocol::{ToolCall, Workspace};
use rmcp::{
    ErrorData, RoleServer, ServerHandler,
    model::{
        CallToolRequestParams, CallToolResponse, CallToolResult, CancelTaskParams,
        CompleteRequestParams, CompleteResult, CompletionInfo, ContentBlock, CreateTaskResult,
        DiscoverResult, ErrorCode, GetPromptRequestParams, GetPromptResponse, GetTaskParams,
        GetTaskResult, InputRequest, ListPromptsResult, ListResourceTemplatesResult,
        ListResourcesResult, ListToolsResult, PaginatedRequestParams, ProtocolVersion,
        ReadResourceRequestParams, ReadResourceResponse, ResourceUpdatedNotificationParam,
        ServerCapabilities, ServerConfig, SubscribeRequestParams, UnsubscribeRequestParams,
        UpdateTaskParams,
    },
    service::RequestContext,
    task_manager::{TaskManager, TaskOptions},
    transport::streamable_http_server::{
        StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
    },
};
use serde_json::{Value, json};
use std::{
    borrow::Cow,
    time::{Duration, Instant},
};

#[derive(Clone)]
struct SdkPeer {
    legacy: bool,
    tasks: TaskManager,
}
impl ServerHandler for SdkPeer {
    fn get_info(&self) -> ServerConfig {
        let capabilities = ServerCapabilities::builder()
            .enable_tools()
            .enable_resources()
            .enable_prompts()
            .enable_completions();
        // A legacy peer can advertise subscribe without speaking subscriptions/listen.
        let capabilities = if self.legacy {
            capabilities.enable_resources_subscribe().build()
        } else {
            capabilities.enable_tasks().build()
        };
        ServerConfig::new(capabilities)
    }
    fn supported_protocol_versions(&self) -> Cow<'static, [ProtocolVersion]> {
        if self.legacy {
            Cow::Borrowed(&[ProtocolVersion::V_2025_11_25])
        } else {
            Cow::Borrowed(&[ProtocolVersion::V_2026_07_28])
        }
    }
    async fn discover(&self, _: RequestContext<RoleServer>) -> Result<DiscoverResult, ErrorData> {
        if self.legacy {
            Err(ErrorData::new(
                ErrorCode::METHOD_NOT_FOUND,
                "legacy fixture",
                None,
            ))
        } else {
            Ok(DiscoverResult::new(
                self.supported_protocol_versions().into_owned(),
                self.get_info().capabilities,
            ))
        }
    }
    async fn list_tools(
        &self,
        _: Option<PaginatedRequestParams>,
        _: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        Ok(serde_json::from_value(json!({"tools":[{"name":"echo","inputSchema":{"type":"object","required":["text"],"properties":{"text":{"type":"string"},"defer":{"type":"boolean"}},"additionalProperties":false}}]})).unwrap())
    }
    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        assert_eq!(request.name, "echo");
        let arguments = request.arguments.unwrap();
        let text = arguments["text"].as_str().unwrap().to_owned();
        if arguments.get("defer") == Some(&json!(true)) {
            assert!(!self.legacy);
            assert!(
                context
                    .client_capabilities()
                    .is_some_and(|caps| caps.supports_tasks())
            );
            let task = self.tasks.spawn(TaskOptions::new().with_poll_interval_ms(10), move |task| {
                Box::pin(async move {
                    let form: InputRequest = serde_json::from_value(json!({"method":"elicitation/create",
                        "params":{"mode":"form","message":"Complete the echo?","requestedSchema":{"type":"object","properties":{}}}})).unwrap();
                    let answer = task.request_input("confirm", form).await?;
                    assert_eq!(answer["action"], "accept");
                    Ok(CallToolResult::success(vec![ContentBlock::text(text)]))
                })
            });
            return Ok(CallToolResponse::Task(CreateTaskResult::new(task)));
        }
        Ok(CallToolResult::success(vec![ContentBlock::text(text)]).into())
    }
    async fn get_task(
        &self,
        request: GetTaskParams,
        _: RequestContext<RoleServer>,
    ) -> Result<GetTaskResult, ErrorData> {
        self.tasks
            .get_task(&request.task_id)
            .map(GetTaskResult::new)
    }
    async fn update_task(
        &self,
        request: UpdateTaskParams,
        _: RequestContext<RoleServer>,
    ) -> Result<(), ErrorData> {
        self.tasks
            .update_task(&request.task_id, request.input_responses)
    }
    async fn cancel_task(
        &self,
        request: CancelTaskParams,
        _: RequestContext<RoleServer>,
    ) -> Result<(), ErrorData> {
        self.tasks.cancel_task(&request.task_id)
    }
    async fn subscribe(
        &self,
        request: SubscribeRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<(), ErrorData> {
        assert!(self.legacy);
        assert_eq!(request.uri, "fixture:guide");
        context
            .peer
            .notify_resource_updated(ResourceUpdatedNotificationParam::new(request.uri))
            .await
            .map_err(|_| ErrorData::internal_error("watch delivery failed", None))
    }
    async fn unsubscribe(
        &self,
        request: UnsubscribeRequestParams,
        _: RequestContext<RoleServer>,
    ) -> Result<(), ErrorData> {
        assert!(self.legacy);
        assert_eq!(request.uri, "fixture:guide");
        Ok(())
    }
    async fn list_resources(
        &self,
        _: Option<PaginatedRequestParams>,
        _: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, ErrorData> {
        Ok(serde_json::from_value(json!({"resources":[{"name":"guide","uri":"fixture:guide"}],"ttlMs":0,"cacheScope":"private"})).unwrap())
    }
    async fn list_resource_templates(
        &self,
        _: Option<PaginatedRequestParams>,
        _: RequestContext<RoleServer>,
    ) -> Result<ListResourceTemplatesResult, ErrorData> {
        Ok(serde_json::from_value(json!({"resourceTemplates":[{"name":"guide","uriTemplate":"fixture:guide/{id}"}],"ttlMs":0,"cacheScope":"private"})).unwrap())
    }
    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        _: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResponse, ErrorData> {
        Ok(serde_json::from_value::<rmcp::model::ReadResourceResult>(json!({"contents":[{"uri":request.uri,"text":"SDK_RESOURCE"}],"ttlMs":0,"cacheScope":"private"})).unwrap().into())
    }
    async fn list_prompts(
        &self,
        _: Option<PaginatedRequestParams>,
        _: RequestContext<RoleServer>,
    ) -> Result<ListPromptsResult, ErrorData> {
        Ok(serde_json::from_value(
            json!({"prompts":[{"name":"draft"}],"ttlMs":0,"cacheScope":"private"}),
        )
        .unwrap())
    }
    async fn get_prompt(
        &self,
        request: GetPromptRequestParams,
        _: RequestContext<RoleServer>,
    ) -> Result<GetPromptResponse, ErrorData> {
        assert_eq!(request.name, "draft");
        Ok(serde_json::from_value::<rmcp::model::GetPromptResult>(
            json!({"messages":[{"role":"user","content":{"type":"text","text":"SDK_PROMPT"}}]}),
        )
        .unwrap()
        .into())
    }
    async fn complete(
        &self,
        request: CompleteRequestParams,
        _: RequestContext<RoleServer>,
    ) -> Result<CompleteResult, ErrorData> {
        assert_eq!(request.r#ref, rmcp::model::Reference::for_prompt("draft"));
        assert_eq!(request.argument.name, "topic");
        assert_eq!(request.argument.value, "SD");
        Ok(CompleteResult::new(
            CompletionInfo::with_all_values(vec!["SDK_TOPIC".into()]).unwrap(),
        ))
    }
}

#[test]
fn official_http_server_interoperates_in_both_lifecycles_and_response_formats() {
    for legacy in [true, false] {
        for json_response in [true, false] {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(1)
                .enable_all()
                .build()
                .unwrap();
            let cancellation = CancellationToken::new();
            let (url, task) = runtime.block_on(async {
                let peer = SdkPeer {
                    legacy,
                    tasks: TaskManager::new(),
                };
                let service: StreamableHttpService<SdkPeer, LocalSessionManager> =
                    StreamableHttpService::new(
                        move || Ok(peer.clone()),
                        Default::default(),
                        StreamableHttpServerConfig::default()
                            .with_legacy_session_mode(legacy)
                            .with_json_response(json_response)
                            .with_sse_keep_alive(None)
                            .with_cancellation_token(cancellation.child_token()),
                    );
                let router = axum::Router::new().nest_service("/mcp", service);
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                let url = format!("http://{}/mcp", listener.local_addr().unwrap());
                let stop = cancellation.clone();
                let task = tokio::spawn(async move {
                    axum::serve(listener, router)
                        .with_graceful_shutdown(stop.cancelled_owned())
                        .await
                        .unwrap();
                });
                (url, task)
            });
            let endpoint = reqwest::Url::parse(&url).unwrap();
            let egress = json!({"routes":[{"host":"127.0.0.1","addresses":["127.0.0.1"],
                "ports":[endpoint.port().unwrap()]}]});
            let config = serde_json::from_value(json!({"id":"sdk","transport":"streamable_http",
                "url":endpoint.as_str(),"timeoutMs":3000,"egress":egress}))
            .unwrap();
            let tools = fliuno_mcp::connect_server(config)
                .unwrap_or_else(|error| panic!("legacy={legacy}, json={json_response}: {error}"));
            let watch = tools
                .iter()
                .find(|tool| tool.spec().name == "mcp_sdk_resources_watch");
            assert_eq!(watch.is_some(), legacy);
            if let Some(watch) = watch {
                let result = watch
                    .execute(
                        &ToolContext {
                            session_id: "session_test",
                            task_id: "sdk-test",
                            workspace: &Workspace {
                                id: "unused".into(),
                                root: "unused".into(),
                            },
                        },
                        &ToolCall {
                            id: "watch".into(),
                            name: watch.spec().name,
                            arguments: json!({"uri":"fixture:guide","waitMs":1000}),
                        },
                        &CancellationToken::new(),
                    )
                    .unwrap_or_else(|error| panic!("legacy watch, json={json_response}: {error}"));
                assert_eq!(result.output["method"], "resources/subscribe");
                assert_eq!(
                    result.output["result"],
                    json!({"uri":"fixture:guide","status":"updated"}),
                    "json={json_response}"
                );
            }
            assert_eq!(
                tools
                    .iter()
                    .any(|tool| tool.spec().name.starts_with("mcp_sdk_tasks_")),
                !legacy
            );
            let result = tools[0]
                .execute(
                    &ToolContext {
                        session_id: "session_test",
                        task_id: "sdk-test",
                        workspace: &Workspace {
                            id: "unused".into(),
                            root: "unused".into(),
                        },
                    },
                    &ToolCall {
                        id: "call".into(),
                        name: tools[0].spec().name,
                        arguments: json!({"text":"官方 SDK 互操作"}),
                    },
                    &CancellationToken::new(),
                )
                .unwrap();
            assert_eq!(result.output["content"][0]["text"], "官方 SDK 互操作");
            if !legacy {
                exercise_sdk_tasks(&tools);
            }
            for (label, arguments, path) in [
                ("resources_list", json!({}), "/resources"),
                ("resources_templates", json!({}), "/resourceTemplates"),
                (
                    "resources_read",
                    json!({"uri":"fixture:guide"}),
                    "/contents",
                ),
                ("prompts_list", json!({}), "/prompts"),
                ("prompts_get", json!({"name":"draft"}), "/messages"),
                (
                    "complete",
                    json!({"ref":{"type":"ref/prompt","name":"draft"},"argument":{"name":"topic","value":"SD"}}),
                    "/completion/values",
                ),
            ] {
                let tool = tools
                    .iter()
                    .find(|tool| tool.spec().name == format!("mcp_sdk_{label}"))
                    .unwrap();
                let result = tool
                    .execute(
                        &ToolContext {
                            session_id: "s",
                            task_id: "t",
                            workspace: &Workspace {
                                id: "w".into(),
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
                    .unwrap();
                let values = result.output["result"]
                    .pointer(path)
                    .unwrap()
                    .as_array()
                    .unwrap();
                assert_eq!(values.len(), 1);
                if label == "complete" {
                    assert_eq!(values[0], "SDK_TOPIC");
                }
                assert_eq!(result.output["grantsPermissions"], false);
            }
            drop(tools);
            cancellation.cancel();
            runtime.block_on(async {
                tokio::time::timeout(Duration::from_secs(5), task)
                    .await
                    .unwrap()
                    .unwrap();
            });
        }
    }
}

fn exercise_sdk_tasks(tools: &[Box<dyn Tool>]) {
    let workspace = Workspace {
        id: "sdk-workspace".into(),
        root: "unused".into(),
    };
    let context = ToolContext {
        workspace: &workspace,
        session_id: "sdk-session",
        task_id: "sdk-task",
    };
    let cancel = CancellationToken::new();
    let invoke = |tool: &dyn Tool, args: Value| {
        tool.execute(
            &context,
            &ToolCall {
                id: "test-call".into(),
                name: tool.spec().name,
                arguments: args,
            },
            &cancel,
        )
        .unwrap_or_else(|err| panic!("{}: {err}", tool.spec().name))
        .output
    };
    let named = |label: &str| {
        let name = format!("mcp_sdk_tasks_{label}");
        tools
            .iter()
            .find(|tool| tool.spec().name == name)
            .unwrap()
            .as_ref()
    };
    let start = || {
        invoke(
            tools[0].as_ref(),
            json!({"text":"completed by SDK","defer":true}),
        )
    };
    let wait_for = |id: &Value, status: &str| {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let value = invoke(named("get"), json!({"taskId":id}));
            if value["result"]["status"] == status {
                return value["result"].clone();
            }
            assert!(Instant::now() < deadline, "expected {status}: {value}");
            std::thread::sleep(Duration::from_millis(10));
        }
    };

    let created = start();
    let id = &created["taskId"];
    assert!(id.is_string());
    let input = wait_for(id, "input_required");
    assert_eq!(
        input["inputRequests"]["confirm"]["method"],
        "elicitation/create"
    );
    invoke(
        named("update"),
        json!({"taskId":id,"inputResponses":{"confirm":{"action":"accept","content":{}}}}),
    );
    let finished = wait_for(id, "completed");
    assert_eq!(finished["result"]["content"][0]["text"], "completed by SDK");

    let created = start();
    let id = &created["taskId"];
    wait_for(id, "input_required");
    invoke(named("cancel"), json!({"taskId":id}));
    wait_for(id, "cancelled");
}
