//! Both sides use the official SDK; raw wire tests separately check rejection
//! and replay boundaries. This test verifies JSON/SSE and lifecycle interop.
use fliuno_core::{CancellationToken, ToolContext};
use fliuno_protocol::{ToolCall, Workspace};
use rmcp::{
    ErrorData, RoleServer, ServerHandler,
    model::{
        CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, DiscoverResult,
        ErrorCode, GetPromptRequestParams, GetPromptResponse, ListPromptsResult,
        ListResourceTemplatesResult, ListResourcesResult, ListToolsResult, PaginatedRequestParams,
        ProtocolVersion, ReadResourceRequestParams, ReadResourceResponse, ServerCapabilities,
        ServerConfig,
    },
    service::RequestContext,
    transport::streamable_http_server::{
        StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
    },
};
use serde_json::json;
use std::{borrow::Cow, time::Duration};

#[derive(Clone)]
struct SdkPeer {
    legacy: bool,
}
impl ServerHandler for SdkPeer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_resources()
                .enable_prompts()
                .build(),
        )
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
        Ok(serde_json::from_value(json!({"tools":[{"name":"echo","inputSchema":{"type":"object","required":["text"],"properties":{"text":{"type":"string"}},"additionalProperties":false}}]})).unwrap())
    }
    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        assert_eq!(request.name, "echo");
        Ok(CallToolResult::success(vec![ContentBlock::text(
            request.arguments.unwrap()["text"].as_str().unwrap(),
        )])
        .into())
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
                let service: StreamableHttpService<SdkPeer, LocalSessionManager> =
                    StreamableHttpService::new(
                        move || Ok(SdkPeer { legacy }),
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
            let config = serde_json::from_value(
                json!({"id":"sdk","transport":"streamable_http","url":url,"timeoutMs":3000}),
            )
            .unwrap();
            let tools = fliuno_mcp::connect_server(config)
                .unwrap_or_else(|error| panic!("legacy={legacy}, json={json_response}: {error}"));
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
            for (label, arguments, field) in [
                ("resources_list", json!({}), "resources"),
                ("resources_templates", json!({}), "resourceTemplates"),
                ("resources_read", json!({"uri":"fixture:guide"}), "contents"),
                ("prompts_list", json!({}), "prompts"),
                ("prompts_get", json!({"name":"draft"}), "messages"),
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
                assert_eq!(result.output["result"][field].as_array().unwrap().len(), 1);
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
