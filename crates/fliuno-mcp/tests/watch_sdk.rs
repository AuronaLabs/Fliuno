//! Interoperate with the official server subscription lifecycle over HTTP SSE.
use fliuno_core::{CancellationToken, Tool, ToolContext, ToolError, ToolOutput};
use fliuno_protocol::{ToolCall, Workspace};
use rmcp::{
    ErrorData, RoleServer, ServerHandler,
    model::{
        DiscoverResult, ProtocolVersion, ResourceUpdatedNotification,
        ResourceUpdatedNotificationParam, ServerCapabilities, ServerConfig, ServerNotification,
        SubscriptionFilter,
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
}

impl ServerHandler for Peer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(
            ServerCapabilities::builder()
                .enable_resources()
                .enable_resources_subscribe()
                .build(),
        )
    }

    fn supported_protocol_versions(&self) -> Cow<'static, [ProtocolVersion]> {
        Cow::Borrowed(&[ProtocolVersion::V_2026_07_28])
    }

    async fn discover(&self, _: RequestContext<RoleServer>) -> Result<DiscoverResult, ErrorData> {
        Ok(DiscoverResult::new(
            self.supported_protocol_versions().into_owned(),
            self.get_info().capabilities,
        ))
    }

    fn accepted_subscription_filter(
        &self,
        requested: &SubscriptionFilter,
    ) -> Option<SubscriptionFilter> {
        let uri = &requested.resource_subscriptions.as_ref().unwrap()[0];
        Some(if uri == "fixture:declined" {
            SubscriptionFilter::new()
        } else {
            requested.clone()
        })
    }

    async fn listen(&self, context: SubscriptionContext) -> Result<(), ErrorData> {
        self.started.fetch_add(1, Ordering::Relaxed);
        if let Some(uris) = &context.accepted().resource_subscriptions {
            assert_eq!(uris.len(), 1);
            if uris[0] == "fixture:ended" {
                self.closed.fetch_add(1, Ordering::Relaxed);
                return Ok(());
            }
            if uris[0] == "fixture:changed" {
                context
                    .sink()
                    .send(ServerNotification::ResourceUpdatedNotification(
                        ResourceUpdatedNotification::new(ResourceUpdatedNotificationParam::new(
                            &uris[0],
                        )),
                    ))
                    .await
                    .unwrap();
            }
        }
        context.cancelled().await;
        self.closed.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }
}

fn connect(
    peer: &Peer,
    stop: &CancellationToken,
) -> (
    tokio::runtime::Runtime,
    tokio::task::JoinHandle<()>,
    Vec<Box<dyn Tool>>,
) {
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
                .with_legacy_session_mode(false)
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
            arguments: json!({"uri":uri,"waitMs":wait_ms}),
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
