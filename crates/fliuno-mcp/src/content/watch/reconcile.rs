//! A separately approved watch can reconcile a current resource snapshot
//! with its durable intent. Only a digest is retained; notifications are hints.
use super::*;
use crate::host::McpService;
use fliuno_core::{McpSubscriptionAttempt, McpSubscriptionKey, McpTaskStore};
use rmcp::model::{
    ClientRequest, ProtocolVersion, ReadResourceRequest, ReadResourceRequestParams, ServerPeerInfo,
    ServerResult,
};

struct Reconcile {
    host: Arc<Host>,
    server: String,
    identity: String,
    store: Arc<McpTaskStore>,
    modern: bool,
    max_wait_ms: u64,
}

pub(in crate::content) fn register(
    host: Arc<Host>,
    server: &str,
    peer: &ServerPeerInfo,
    store: Arc<McpTaskStore>,
) -> Box<dyn Tool> {
    let identity = fliuno_core::authority_digest(
        "mcp.resourceSubscription.v1",
        &json!({
            "host":host.authority, "protocolVersion":peer.protocol_version, "serverInfo":peer.server_info,
        }),
    );
    let max_wait_ms = MAX_WAIT_MS.min(host.timeout.as_millis() as u64 / 2);
    Box::new(Reconcile {
        host,
        server: server.into(),
        identity,
        store,
        max_wait_ms,
        modern: peer.protocol_version >= ProtocolVersion::V_2026_07_28,
    })
}

impl Reconcile {
    fn args(&self, value: &Value) -> Result<ResourceArgs, String> {
        let invalid = || "invalid MCP resource reconciliation URI or waitMs".to_owned();
        let args = ResourceArgs::deserialize(value).map_err(|_| invalid())?;
        if !arguments::valid_uri(&args.uri) || !(1..=self.max_wait_ms).contains(&args.wait_ms) {
            return Err(invalid());
        }
        Ok(args)
    }
}

impl Tool for Reconcile {
    fn authority(&self) -> String {
        fliuno_core::authority_digest(
            "mcp.resourceSubscription.tool.v1",
            &json!({
                "identity":self.identity, "schema":self.spec().input_schema,
            }),
        )
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: format!("mcp_{}_resources_watch_reconcile", self.server),
            description: "Subscribe to one resource, read its current snapshot and compare it with this session's last approved observation. Only status is returned. A notification may trigger one additional read.".into(),
            effect: ToolEffect::External,
            input_schema: json!({"type":"object","required":["uri","waitMs"],"additionalProperties":false,
                "properties":{"uri":{"type":"string","minLength":1,"maxLength":4096},
                    "waitMs":{"type":"integer","minimum":1,"maximum":self.max_wait_ms}}}),
        }
    }

    fn validate(&self, value: &Value) -> Result<(), String> {
        self.args(value).map(drop)
    }

    fn execute(
        &self,
        context: &ToolContext<'_>,
        call: &ToolCall,
        cancellation: &CancellationToken,
    ) -> Result<ToolOutput, ToolError> {
        let ResourceArgs { uri, wait_ms } = self.args(&call.arguments)?;
        let watched_uri = uri.clone();
        let workspace = context.workspace.id.clone();
        let session = context.session_id.to_owned();
        let store = Arc::clone(&self.store);
        let identity = self.identity.clone();
        let modern = self.modern;
        let result = self.host.request(cancellation, move |service| {
            Box::pin(async move {
                let key = McpSubscriptionKey {
                    workspace_id: &workspace,
                    session_id: &session,
                    server_identity: &identity,
                    uri: &watched_uri,
                };
                let attempt = match store.begin_resource_subscription(&key) {
                    Ok(attempt) => attempt,
                    Err(_) => {
                        return Ok(Some(Err(ToolError::Failed(
                            "cannot begin MCP subscription in this session".into(),
                        ))));
                    }
                };
                let wait = Duration::from_millis(wait_ms);
                let observation = if modern {
                    observe_modern(service, &watched_uri, wait, &attempt).await?
                } else {
                    observe_legacy(service, &watched_uri, wait, &attempt).await?
                };
                let Some(observation) = observation else {
                    return Ok(None);
                };
                if store
                    .finish_resource_subscription(&key, &attempt, observation.snapshot.as_deref())
                    .is_err()
                {
                    return Ok(Some(Err(ToolError::OutcomeUnknown(
                        "cannot record MCP subscription observation".into(),
                    ))));
                }
                Ok(Some(Ok(
                    json!({"status":observation.status, "disconnected":attempt.was_disconnected}),
                )))
            })
        })??;
        let mut result = result;
        result["uri"] = json!(uri);
        Ok(ToolOutput {
            output: json!({"serverId":self.server,"method":if modern {METHOD} else {LEGACY_METHOD},
                "result":result,"grantsPermissions":false}),
            artifacts: Vec::new(),
        })
    }
}

struct Observation {
    status: &'static str,
    snapshot: Option<String>,
}

impl Observation {
    fn without_snapshot(status: &'static str) -> Self {
        Self {
            status,
            snapshot: None,
        }
    }
    fn snapshot(status: &'static str, snapshot: String) -> Self {
        Self {
            status,
            snapshot: Some(snapshot),
        }
    }
}

async fn read_snapshot(service: &McpService, uri: &str) -> Result<Option<String>, ServiceError> {
    // Send a direct request so TTL cache does not hide a change during an outage.
    let request = ClientRequest::ReadResourceRequest(ReadResourceRequest::new(
        ReadResourceRequestParams::new(uri),
    ));
    let ServerResult::ReadResourceResult(result) = service.peer().send_request(request).await?
    else {
        return Ok(None);
    };
    if !crate::host::is_complete(&result.result_type)
        || result.contents.len() > 64
        || !fliuno_core::json_size(&result.contents).is_ok_and(|size| size <= 1024 * 1024)
    {
        return Ok(None);
    }
    let contents =
        serde_json::to_value(result.contents).map_err(|_| ServiceError::UnexpectedResponse)?;
    Ok(Some(fliuno_core::authority_digest(
        "mcp.resource.snapshot.v1",
        &contents,
    )))
}

async fn observe_modern(
    service: &McpService,
    uri: &str,
    wait: Duration,
    attempt: &McpSubscriptionAttempt,
) -> Result<Option<Observation>, ServiceError> {
    let target = Target::ResourceUri(uri.to_owned());
    let filter = target.filter();
    let mut subscription = service
        .listen_with_capacity(filter.clone(), CAPACITY)
        .await?;
    if !filter.is_subset_of(subscription.acknowledged()) {
        subscription.cancel().await?;
        return Ok(Some(Observation::without_snapshot("declined")));
    }
    let result = observe(service, uri, attempt, async {
        match tokio::time::timeout(wait, subscription.next()).await {
            Err(_) => Ok(Some("elapsed")),
            Ok(Ok(Some(notification))) if target.matches(&notification) => Ok(Some("observed")),
            Ok(Ok(None)) if matches!(subscription.end(), Some(SubscriptionEnd::Graceful(_))) => {
                Ok(Some("ended"))
            }
            Ok(Err(error)) => Err(error),
            _ => Ok(None),
        }
    })
    .await;
    subscription.cancel().await?;
    result
}

#[allow(deprecated)]
async fn observe_legacy(
    service: &McpService,
    uri: &str,
    wait: Duration,
    attempt: &McpSubscriptionAttempt,
) -> Result<Option<Observation>, ServiceError> {
    let mut updates = service.service().legacy_resource_updates.subscribe();
    match service.subscribe(SubscribeRequestParams::new(uri)).await {
        Ok(()) => {}
        Err(ServiceError::McpError(_)) => {
            return Ok(Some(Observation::without_snapshot("declined")));
        }
        Err(error) => return Err(error),
    }
    let result = observe(service, uri, attempt, async {
        Ok(wait_for_legacy_update(&mut updates, uri, wait)
            .await
            .ok()
            .map(|notified| if notified { "observed" } else { "elapsed" }))
    })
    .await;
    service
        .unsubscribe(UnsubscribeRequestParams::new(uri))
        .await?;
    result
}

async fn observe(
    service: &McpService,
    uri: &str,
    attempt: &McpSubscriptionAttempt,
    notification: impl Future<Output = Result<Option<&'static str>, ServiceError>>,
) -> Result<Option<Observation>, ServiceError> {
    let Some(current) = read_snapshot(service, uri).await? else {
        return Ok(None);
    };
    if attempt
        .previous_snapshot
        .as_ref()
        .is_some_and(|last| last != &current)
    {
        return Ok(Some(Observation::snapshot("changed", current)));
    }
    let Some(status) = notification.await? else {
        return Ok(None);
    };
    if status != "observed" {
        return Ok(Some(Observation::snapshot(status, current)));
    }
    let Some(after) = read_snapshot(service, uri).await? else {
        return Ok(None);
    };
    let status = if after == current {
        "unchanged"
    } else {
        "changed"
    };
    Ok(Some(Observation::snapshot(status, after)))
}
