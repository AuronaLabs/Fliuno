//! Interoperate with the official server subscription lifecycle over HTTP SSE.
use fliuno_core::{CancellationToken, Tool, ToolContext, ToolError, ToolOutput};
use fliuno_protocol::{ToolCall, Workspace};
use rmcp::{
    ErrorData, RoleServer, ServerHandler,
    model::{
        DiscoverResult, ListToolsResult, PaginatedRequestParams, ProtocolVersion,
        ResourceUpdatedNotification, ResourceUpdatedNotificationParam, ServerCapabilities,
        ServerConfig, ServerNotification, SubscriptionFilter,
    },
    service::{RequestContext, SubscriptionContext},
    transport::streamable_http_server::{
        StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
    },
};
use serde_json::json;
use std::{
    borrow::Cow,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

#[derive(Clone, Default)]
struct Peer {
    started: Arc<AtomicUsize>,
    closed: Arc<AtomicUsize>,
    listed: Arc<AtomicUsize>,
    cache_catalog: bool,
    legacy: bool,
    resource_version: Arc<AtomicUsize>,
    resource_reads: Arc<AtomicUsize>,
    fail_read: Arc<std::sync::atomic::AtomicBool>,
}

impl ServerHandler for Peer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(
            ServerCapabilities::builder()
                .enable_resources()
                .enable_resources_subscribe()
                .enable_resources_list_changed()
                .enable_prompts()
                .enable_prompts_list_changed()
                .enable_tools()
                .enable_tool_list_changed()
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
        Ok(DiscoverResult::new(
            self.supported_protocol_versions().into_owned(),
            self.get_info().capabilities,
        ))
    }

    async fn list_tools(
        &self,
        page: Option<PaginatedRequestParams>,
        _: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        let call = self.listed.fetch_add(1, Ordering::Relaxed);
        let mut result = match (call, page.and_then(|params| params.cursor)) {
            (0, None) => json!({"tools":[]}),
            (_, None) => {
                json!({"tools":[{"name":"added_later","inputSchema":{"type":"object"}}],"nextCursor":"next"})
            }
            (_, Some(cursor)) if cursor == "next" => json!({"tools":[]}),
            _ => return Err(ErrorData::invalid_params("unexpected page", None)),
        };
        if self.cache_catalog {
            result["ttlMs"] = json!(60_000);
            result["cacheScope"] = json!("private");
        }
        Ok(serde_json::from_value(result).unwrap())
    }

    async fn read_resource(
        &self,
        request: rmcp::model::ReadResourceRequestParams,
        _: RequestContext<RoleServer>,
    ) -> Result<rmcp::model::ReadResourceResponse, ErrorData> {
        self.resource_reads.fetch_add(1, Ordering::Relaxed);
        if self.fail_read.load(Ordering::Relaxed) {
            return Err(ErrorData::internal_error("fixture unavailable", None));
        }
        let version = self.resource_version.load(Ordering::Relaxed);
        Ok(
            serde_json::from_value::<rmcp::model::ReadResourceResult>(json!({
                "contents":[{"uri":request.uri,"text":format!("PRIVATE_FIXTURE_{version}")}],
                "ttlMs":60000,"cacheScope":"private"
            }))
            .unwrap()
            .into(),
        )
    }

    async fn subscribe(
        &self,
        request: rmcp::model::SubscribeRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<(), ErrorData> {
        assert!(self.legacy);
        self.started.fetch_add(1, Ordering::Relaxed);
        context
            .peer
            .notify_resource_updated(ResourceUpdatedNotificationParam::new(request.uri))
            .await
            .map_err(|_| ErrorData::internal_error("watch delivery failed", None))
    }

    async fn unsubscribe(
        &self,
        _: rmcp::model::UnsubscribeRequestParams,
        _: RequestContext<RoleServer>,
    ) -> Result<(), ErrorData> {
        assert!(self.legacy);
        self.closed.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    fn accepted_subscription_filter(
        &self,
        requested: &SubscriptionFilter,
    ) -> Option<SubscriptionFilter> {
        let mut accepted = requested.clone();
        if let Some(uris) = &mut accepted.resource_subscriptions {
            uris.retain(|uri| uri != "fixture:declined");
            if uris.is_empty() {
                accepted.resource_subscriptions = None;
            }
        }
        Some(accepted)
    }

    async fn listen(&self, context: SubscriptionContext) -> Result<(), ErrorData> {
        self.started.fetch_add(1, Ordering::Relaxed);
        if let Some(uris) = &context.accepted().resource_subscriptions {
            if uris == &["fixture:ended"] {
                self.closed.fetch_add(1, Ordering::Relaxed);
                return Ok(());
            }
            if let Some(changed) = uris.iter().find(|uri| *uri == "fixture:changed") {
                context
                    .sink()
                    .send(ServerNotification::ResourceUpdatedNotification(
                        ResourceUpdatedNotification::new(ResourceUpdatedNotificationParam::new(
                            changed,
                        )),
                    ))
                    .await
                    .unwrap();
            }
        } else if context.accepted().resources_list_changed == Some(true) {
            context.sink().notify_resource_list_changed().await.unwrap();
        } else if context.accepted().prompts_list_changed == Some(true) {
            context.sink().notify_prompt_list_changed().await.unwrap();
        } else if context.accepted().tools_list_changed == Some(true) {
            context.sink().notify_tool_list_changed().await.unwrap();
        }
        context.cancelled().await;
        self.closed.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }
}

fn start_peer(
    peer: &Peer,
    stop: &CancellationToken,
) -> (tokio::runtime::Runtime, tokio::task::JoinHandle<()>, String) {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .unwrap();
    let (url, server) = runtime.block_on(async {
        let fixture = peer.clone();
        let service: StreamableHttpService<Peer, LocalSessionManager> = StreamableHttpService::new(
            move || Ok(fixture.clone()),
            Default::default(),
            StreamableHttpServerConfig::default()
                .with_legacy_session_mode(peer.legacy)
                .with_sse_keep_alive(None)
                .with_cancellation_token(stop.child_token()),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/mcp", listener.local_addr().unwrap());
        let shutdown = stop.clone();
        let server = tokio::spawn(async move {
            axum::serve(listener, axum::Router::new().nest_service("/mcp", service))
                .with_graceful_shutdown(shutdown.cancelled_owned())
                .await
                .unwrap();
        });
        (url, server)
    });
    (runtime, server, url)
}

fn connect(
    peer: &Peer,
    stop: &CancellationToken,
) -> (
    tokio::runtime::Runtime,
    tokio::task::JoinHandle<()>,
    Vec<Box<dyn Tool>>,
) {
    let (runtime, server, url) = start_peer(peer, stop);
    let tools = fliuno_mcp::connect_server(
        serde_json::from_value(json!({
            "id":"sdk","transport":"streamable_http","url":url,"timeoutMs":3000
        }))
        .unwrap(),
    )
    .unwrap();
    (runtime, server, tools)
}

fn watch_tool(tools: &[Box<dyn Tool>]) -> &dyn Tool {
    tools
        .iter()
        .find(|tool| tool.spec().name == "mcp_sdk_resources_watch")
        .unwrap()
        .as_ref()
}

fn watch(
    tool: &dyn Tool,
    uri: &str,
    wait_ms: u64,
    cancellation: &CancellationToken,
) -> Result<ToolOutput, ToolError> {
    call_watch(tool, json!({"uri":uri,"waitMs":wait_ms}), cancellation)
}

fn call_watch(
    tool: &dyn Tool,
    arguments: serde_json::Value,
    cancellation: &CancellationToken,
) -> Result<ToolOutput, ToolError> {
    let workspace = Workspace {
        id: "w".into(),
        root: "unused".into(),
    };
    tool.execute(
        &ToolContext {
            session_id: "s",
            task_id: "t",
            workspace: &workspace,
        },
        &ToolCall {
            id: "call".into(),
            name: tool.spec().name,
            arguments,
        },
        cancellation,
    )
}

fn wait_for(counter: &AtomicUsize, value: usize) {
    let deadline = Instant::now() + Duration::from_secs(2);
    while counter.load(Ordering::Relaxed) < value {
        assert!(
            Instant::now() < deadline,
            "server did not reach count {value}"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn a_bounded_watch_closes_each_stream_and_keeps_the_connection_reusable() {
    let peer = Peer::default();
    let stop = CancellationToken::new();
    let (runtime, server, tools) = connect(&peer, &stop);
    let tool = watch_tool(&tools);
    assert!(
        tool.validate(&json!({"uri":"fixture:changed","waitMs":1501}))
            .is_err()
    );
    assert_eq!(peer.started.load(Ordering::Relaxed), 0);
    for (uri, expected, wait_ms) in [
        ("fixture:changed", "updated", 1000),
        ("fixture:idle", "elapsed", 20),
        ("fixture:declined", "declined", 1000),
        ("fixture:ended", "ended", 1000),
        ("fixture:changed", "updated", 1000),
    ] {
        let result = watch(tool, uri, wait_ms, &CancellationToken::new())
            .unwrap_or_else(|err| panic!("{uri}: {err}"));
        assert_eq!(
            result.output["result"],
            json!({"uri":uri,"status":expected})
        );
        assert_eq!(result.output["grantsPermissions"], false);
        assert!(result.artifacts.is_empty());
    }
    wait_for(&peer.closed, 5);
    assert_eq!(peer.started.load(Ordering::Relaxed), 5);
    drop(tools);
    stop.cancel();
    runtime.block_on(server).unwrap();
}

#[test]
fn one_watch_reports_the_changed_uri_and_declines_a_partial_acknowledgement() {
    let peer = Peer::default();
    let stop = CancellationToken::new();
    let (runtime, server, tools) = connect(&peer, &stop);
    let tool = tools
        .iter()
        .find(|tool| tool.spec().name == "mcp_sdk_resources_watch_many")
        .unwrap();
    let cancel = CancellationToken::new();
    for uris in [
        json!([]),
        json!(["fixture:idle", "fixture:idle"]),
        json!(["fixture:idle", "not a URI"]),
        json!((0..9).map(|i| format!("fixture:{i}")).collect::<Vec<_>>()),
    ] {
        assert!(call_watch(tool.as_ref(), json!({"uris":uris,"waitMs":1000}), &cancel).is_err());
    }
    assert_eq!(peer.started.load(Ordering::Relaxed), 0);
    for (uris, expected) in [
        (
            ["fixture:idle", "fixture:changed"],
            json!({"status":"updated","updatedUri":"fixture:changed"}),
        ),
        (
            ["fixture:declined", "fixture:idle"],
            json!({"status":"declined"}),
        ),
    ] {
        let result =
            call_watch(tool.as_ref(), json!({"uris":uris,"waitMs":1000}), &cancel).unwrap();
        assert_eq!(result.output["result"], expected);
        assert_eq!(result.output["grantsPermissions"], false);
        assert!(result.artifacts.is_empty());
    }
    wait_for(&peer.closed, 2);
    assert_eq!(peer.started.load(Ordering::Relaxed), 2);
    drop(tools);
    stop.cancel();
    runtime.block_on(server).unwrap();
}

#[test]
fn cancellation_after_dispatch_closes_the_host_without_resending() {
    let peer = Peer::default();
    let stop = CancellationToken::new();
    let (runtime, server, tools) = connect(&peer, &stop);
    let tool = watch_tool(&tools);
    let cancellation = CancellationToken::new();
    std::thread::scope(|scope| {
        let call = scope.spawn(|| watch(tool, "fixture:idle", 1500, &cancellation));
        wait_for(&peer.started, 1);
        cancellation.cancel();
        assert!(
            matches!(call.join().unwrap(), Err(ToolError::OutcomeUnknown(message))
            if message.contains("cancelled"))
        );
    });
    wait_for(&peer.closed, 1);
    assert!(
        matches!(watch(tool, "fixture:changed", 500, &CancellationToken::new()),
        Err(ToolError::Failed(message)) if message.contains("closed"))
    );
    assert_eq!(peer.started.load(Ordering::Relaxed), 1);
    drop(tools);
    stop.cancel();
    runtime.block_on(server).unwrap();
}

#[test]
fn catalog_watches_observe_one_advertised_list_and_release_the_stream() {
    let peer = Peer::default();
    let stop = CancellationToken::new();
    let (runtime, server, tools) = connect(&peer, &stop);
    for (catalog, expected) in [
        ("resources", "updated"),
        ("prompts", "updated"),
        ("tools", "updated"),
    ] {
        let name = format!("mcp_sdk_{catalog}_watch_list");
        let tool = tools.iter().find(|tool| tool.spec().name == name).unwrap();
        assert!(tool.validate(&json!({"uri":null,"waitMs":1000})).is_err());
        let result = call_watch(
            tool.as_ref(),
            json!({"waitMs":1000}),
            &CancellationToken::new(),
        )
        .unwrap_or_else(|err| panic!("{catalog}: {err}"));
        assert_eq!(
            result.output,
            json!({"serverId":"sdk","method":"subscriptions/listen", "result":{"status":expected},"grantsPermissions":false})
        );
        assert!(result.artifacts.is_empty());
    }
    wait_for(&peer.closed, 3);
    assert_eq!(peer.started.load(Ordering::Relaxed), 3);
    drop(tools);
    stop.cancel();
    runtime.block_on(server).unwrap();
}

#[test]
fn tools_change_notification_invalidates_cached_catalog_without_reregistering_tools() {
    let peer = Peer {
        cache_catalog: true,
        ..Peer::default()
    };
    let stop = CancellationToken::new();
    let (runtime, server, tools) = connect(&peer, &stop);
    let tool = |name| {
        tools
            .iter()
            .find(|tool| tool.spec().name == name)
            .unwrap()
            .as_ref()
    };
    let cancel = CancellationToken::new();
    let catalog = tool("mcp_sdk_tools_list");
    let before = call_watch(catalog, json!({}), &cancel).unwrap();
    assert_eq!(before.output["result"]["tools"], json!([]));
    assert_eq!(peer.listed.load(Ordering::Relaxed), 1);
    let watch = tool("mcp_sdk_tools_watch_list");
    assert_eq!(
        call_watch(watch, json!({"waitMs":500}), &cancel)
            .unwrap()
            .output["result"]["status"],
        "updated"
    );
    let after = call_watch(catalog, json!({}), &cancel).unwrap();
    assert_eq!(after.output["result"]["tools"][0]["name"], "added_later");
    assert_eq!(peer.listed.load(Ordering::Relaxed), 2);
    assert!(
        tools
            .iter()
            .all(|tool| !tool.spec().name.contains("added_later"))
    );
    drop(tools);
    stop.cancel();
    runtime.block_on(server).unwrap();
}

#[test]
fn remote_tool_pages_are_data_and_leave_startup_registrations_unchanged() {
    let peer = Peer::default();
    let stop = CancellationToken::new();
    let (runtime, server, tools) = connect(&peer, &stop);
    let catalog = tools
        .iter()
        .find(|tool| tool.spec().name == "mcp_sdk_tools_list")
        .unwrap();
    assert_eq!(peer.listed.load(Ordering::Relaxed), 1);
    let cancel = CancellationToken::new();
    let first = call_watch(catalog.as_ref(), json!({}), &cancel).unwrap();
    assert_eq!(peer.listed.load(Ordering::Relaxed), 2);
    assert_eq!(first.output["method"], "tools/list");
    assert_eq!(first.output["grantsPermissions"], false);
    assert_eq!(first.output["result"]["tools"][0]["name"], "added_later");
    assert_eq!(first.output["result"]["nextCursor"], "next");
    assert!(
        tools
            .iter()
            .all(|tool| !tool.spec().name.contains("added_later"))
    );
    let last = call_watch(catalog.as_ref(), json!({"cursor":"next"}), &cancel).unwrap();
    assert_eq!(peer.listed.load(Ordering::Relaxed), 3);
    assert_eq!(last.output["result"]["tools"], json!([]));
    drop(tools);
    stop.cancel();
    runtime.block_on(server).unwrap();
}

#[test]
fn a_durable_watch_reconciles_after_disconnect_and_isolates_session_baselines() {
    for legacy in [false, true] {
        assert_durable_reconciliation(legacy);
    }
}

fn assert_durable_reconciliation(legacy: bool) {
    use fliuno_core::{Kernel, ModelDecision, ModelProvider, ModelTurn, ToolSpec};
    use fliuno_protocol::{ApprovalDecision, Message, Task, TaskStatus};

    struct Provider;
    impl ModelProvider for Provider {
        fn next(
            &self,
            task: &Task,
            _: &[ToolSpec],
            _: &CancellationToken,
        ) -> Result<ModelTurn, String> {
            Ok(if task
                .messages
                .iter()
                .any(|m| matches!(m, Message::ToolResult { .. }))
            {
                ModelDecision::Complete("observed".into())
            } else {
                ModelDecision::Call(ToolCall {
                    id: format!("watch_{}", task.id),
                    name: "mcp_sdk_resources_watch_reconcile".into(),
                    arguments: json!({"uri":"fixture:changed","waitMs":200}),
                })
            }
            .into())
        }
    }

    let peer = Peer {
        legacy,
        ..Peer::default()
    };
    let stop = CancellationToken::new();
    let (runtime, server, url) = start_peer(&peer, &stop);
    let dir = std::env::temp_dir().join(format!("fliuno-watch-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&dir).unwrap();
    let db = dir.join("kernel.db");
    let open = || {
        let mut kernel = Kernel::open(&db, Box::new(Provider)).unwrap();
        let config = serde_json::from_value(json!({"servers":[{
            "id":"sdk","transport":"streamable_http","url":url,"timeoutMs":3000,
            "lifecycle": if legacy { "legacy" } else { "discover" },
        }]}))
        .unwrap();
        fliuno_mcp::register_config(&mut kernel, config).unwrap();
        kernel
    };
    let approve = |kernel: &mut Kernel, session_id: &str, decision, expected_reads| {
        let task = kernel.start_task(session_id, "observe resource").unwrap();
        assert_eq!(peer.resource_reads.load(Ordering::Relaxed), expected_reads);
        kernel
            .respond_approval(
                &task.id,
                &task.pending_approval.as_ref().unwrap().id,
                decision,
            )
            .unwrap()
    };
    fn result(task: &Task, status: &str, disconnected: bool) {
        assert_eq!(task.status, TaskStatus::Completed);
        let output = task
            .messages
            .iter()
            .find_map(|m| match m {
                Message::ToolResult { output, .. } => Some(output),
                _ => None,
            })
            .unwrap();
        assert_eq!(
            output["result"],
            json!({"uri":"fixture:changed","status":status,"disconnected":disconnected})
        );
        assert!(!output.to_string().contains("PRIVATE_FIXTURE_"));
    }

    let mut kernel = open();
    let workspace = kernel.open_workspace(dir.to_str().unwrap()).unwrap();
    let session = kernel.create_session(&workspace.id).unwrap();
    let other = kernel.create_session(&workspace.id).unwrap();
    assert_eq!(
        approve(&mut kernel, &session.id, ApprovalDecision::Deny, 0).status,
        TaskStatus::Failed
    );
    assert_eq!(peer.started.load(Ordering::Relaxed), 0);
    result(
        &approve(&mut kernel, &session.id, ApprovalDecision::AllowOnce, 0),
        "unchanged",
        false,
    );
    assert_eq!(peer.resource_reads.load(Ordering::Relaxed), 2);
    peer.fail_read.store(true, Ordering::Relaxed);
    assert_eq!(
        approve(&mut kernel, &session.id, ApprovalDecision::AllowOnce, 2).status,
        TaskStatus::Interrupted
    );
    assert_eq!(peer.resource_reads.load(Ordering::Relaxed), 3);
    drop(kernel);

    peer.resource_version.store(1, Ordering::Relaxed);
    peer.fail_read.store(false, Ordering::Relaxed);
    let mut kernel = open();
    result(
        &approve(&mut kernel, &other.id, ApprovalDecision::AllowOnce, 3),
        "unchanged",
        false,
    );
    assert_eq!(peer.resource_reads.load(Ordering::Relaxed), 5);
    result(
        &approve(&mut kernel, &session.id, ApprovalDecision::AllowOnce, 5),
        "changed",
        true,
    );
    assert_eq!(peer.resource_reads.load(Ordering::Relaxed), 6);
    result(
        &approve(&mut kernel, &session.id, ApprovalDecision::AllowOnce, 6),
        "unchanged",
        false,
    );
    assert_eq!(peer.resource_reads.load(Ordering::Relaxed), 8);
    wait_for(&peer.closed, 5);
    drop(kernel);
    stop.cancel();
    runtime.block_on(server).unwrap();
    std::fs::remove_file(&db).unwrap();
    std::fs::remove_file(dir.join("kernel.db.lock")).unwrap();
    std::fs::remove_dir(dir).unwrap();
}
