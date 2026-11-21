//! An approved watch observes at most one notification from one subscription.
use super::{Host, Tool, ToolContext, ToolError, ToolOutput, ToolSpec, arguments, authority};
use fliuno_core::{CancellationToken, ToolEffect};
use fliuno_protocol::ToolCall;
use rmcp::{
    ServiceError,
    model::{
        ServerCapabilities, ServerNotification, SubscribeRequestParams, SubscriptionFilter,
        UnsubscribeRequestParams,
    },
    service::SubscriptionEnd,
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{num::NonZeroUsize, sync::Arc, time::Duration};

pub(super) mod reconcile;

const METHOD: &str = "subscriptions/listen";
const LEGACY_METHOD: &str = "resources/subscribe";
const CAPACITY: NonZeroUsize = NonZeroUsize::new(8).unwrap();
const MAX_WAIT_MS: u64 = 5_000;
const MAX_RESOURCE_URIS: usize = 8;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ResourceArgs {
    uri: String,
    wait_ms: u64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CatalogArgs {
    wait_ms: u64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ResourceSetArgs {
    uris: Vec<String>,
    wait_ms: u64,
}

#[derive(Clone, Copy)]
enum Kind {
    ResourceUri,
    ResourceSet,
    Catalog(Catalog),
}

#[derive(Clone, Copy)]
enum Catalog {
    Resources,
    Prompts,
    Tools,
}

impl Kind {
    fn label(self) -> &'static str {
        match self {
            Self::ResourceUri => "resources_watch",
            Self::ResourceSet => "resources_watch_many",
            Self::Catalog(Catalog::Resources) => "resources_watch_list",
            Self::Catalog(Catalog::Prompts) => "prompts_watch_list",
            Self::Catalog(Catalog::Tools) => "tools_watch_list",
        }
    }

    fn description(self) -> &'static str {
        match self {
            Self::ResourceUri => {
                "Watch one remote resource URI for one update during the approved wait. The notification contains no fetched resource body."
            }
            Self::ResourceSet => {
                "Watch up to 8 remote resource URIs and report the first update. No resource body is fetched."
            }
            Self::Catalog(Catalog::Resources) => {
                "Watch the remote resource catalog for one change. List resources separately to fetch the current catalog."
            }
            Self::Catalog(Catalog::Prompts) => {
                "Watch the remote prompt catalog for one change. List prompts separately to fetch the current catalog."
            }
            Self::Catalog(Catalog::Tools) => {
                "Watch the remote tool catalog for one change. List tools separately to inspect the current catalog."
            }
        }
    }
}

#[derive(Clone)]
enum Target {
    ResourceUri(String),
    ResourceSet(Vec<String>),
    Catalog(Catalog),
}

impl Target {
    fn filter(&self) -> SubscriptionFilter {
        let builder = SubscriptionFilter::builder();
        match self {
            Self::ResourceUri(uri) => builder.resource_subscription(uri),
            Self::ResourceSet(uris) => builder.resource_subscriptions(uris),
            Self::Catalog(Catalog::Resources) => builder.resources_list_changed(),
            Self::Catalog(Catalog::Prompts) => builder.prompts_list_changed(),
            Self::Catalog(Catalog::Tools) => builder.tools_list_changed(),
        }
        .build()
    }

    fn matches(&self, notification: &ServerNotification) -> bool {
        match (self, notification) {
            (Self::ResourceUri(uri), ServerNotification::ResourceUpdatedNotification(update)) => {
                update.params.uri == *uri
            }
            (Self::ResourceSet(uris), ServerNotification::ResourceUpdatedNotification(update)) => {
                uris.contains(&update.params.uri)
            }
            (
                Self::Catalog(Catalog::Resources),
                ServerNotification::ResourceListChangedNotification(_),
            )
            | (
                Self::Catalog(Catalog::Prompts),
                ServerNotification::PromptListChangedNotification(_),
            )
            | (Self::Catalog(Catalog::Tools), ServerNotification::ToolListChangedNotification(_)) => {
                true
            }
            _ => false,
        }
    }

    fn result(&self, status: &str, updated_uri: Option<String>) -> Value {
        match self {
            Self::ResourceUri(uri) => json!({"uri":uri,"status":status}),
            Self::ResourceSet(_) if updated_uri.is_some() => {
                json!({"status":status,"updatedUri":updated_uri})
            }
            Self::ResourceSet(_) | Self::Catalog(_) => json!({"status":status}),
        }
    }
}

pub(super) fn register(
    host: Arc<Host>,
    server: &str,
    caps: &ServerCapabilities,
    modern: bool,
) -> Vec<Box<dyn Tool>> {
    // Keep part of the call deadline for opening and closing the subscription.
    let max_wait_ms = MAX_WAIT_MS.min(host.timeout.as_millis() as u64 / 2);
    [
        (
            Kind::ResourceUri,
            caps.resources.as_ref().and_then(|r| r.subscribe),
        ),
        (
            Kind::ResourceSet,
            caps.resources.as_ref().and_then(|r| r.subscribe),
        ),
        (
            Kind::Catalog(Catalog::Resources),
            caps.resources.as_ref().and_then(|r| r.list_changed),
        ),
        (
            Kind::Catalog(Catalog::Prompts),
            caps.prompts.as_ref().and_then(|p| p.list_changed),
        ),
        (
            Kind::Catalog(Catalog::Tools),
            caps.tools.as_ref().and_then(|t| t.list_changed),
        ),
    ]
    .into_iter()
    .filter(|(kind, enabled)| {
        *enabled == Some(true) && (modern || matches!(kind, Kind::ResourceUri))
    })
    .map(|(kind, _)| {
        Box::new(Watch {
            host: Arc::clone(&host),
            kind,
            server: server.into(),
            max_wait_ms,
            modern,
        }) as Box<dyn Tool>
    })
    .collect()
}

struct Watch {
    host: Arc<Host>,
    kind: Kind,
    server: String,
    max_wait_ms: u64,
    modern: bool,
}

impl Watch {
    fn method(&self) -> &'static str {
        if self.modern { METHOD } else { LEGACY_METHOD }
    }

    fn args(&self, value: &Value) -> Result<(Target, u64), String> {
        let invalid = || match self.kind {
            Kind::ResourceUri => "invalid MCP watch URI or waitMs".to_owned(),
            Kind::ResourceSet => "invalid MCP watch URIs or waitMs".to_owned(),
            Kind::Catalog(_) => "invalid MCP catalog watch waitMs".to_owned(),
        };
        let (target, wait_ms) = match self.kind {
            Kind::ResourceUri => {
                let args = ResourceArgs::deserialize(value).map_err(|_| invalid())?;
                if !arguments::valid_uri(&args.uri) {
                    return Err(invalid());
                }
                (Target::ResourceUri(args.uri), args.wait_ms)
            }
            Kind::ResourceSet => {
                let args = ResourceSetArgs::deserialize(value).map_err(|_| invalid())?;
                if !(1..=MAX_RESOURCE_URIS).contains(&args.uris.len())
                    || args.uris.iter().enumerate().any(|(index, uri)| {
                        !arguments::valid_uri(uri) || args.uris[..index].contains(uri)
                    })
                {
                    return Err(invalid());
                }
                (Target::ResourceSet(args.uris), args.wait_ms)
            }
            Kind::Catalog(catalog) => {
                let args = CatalogArgs::deserialize(value).map_err(|_| invalid())?;
                (Target::Catalog(catalog), args.wait_ms)
            }
        };
        if !(1..=self.max_wait_ms).contains(&wait_ms) {
            return Err(invalid());
        }
        Ok((target, wait_ms))
    }
}

impl Tool for Watch {
    fn authority(&self) -> String {
        authority(&self.host, &self.spec(), self.method())
    }

    fn spec(&self) -> ToolSpec {
        let mut required = vec!["waitMs"];
        let mut properties =
            json!({"waitMs":{"type":"integer","minimum":1,"maximum":self.max_wait_ms}});
        match self.kind {
            Kind::ResourceUri => {
                required.insert(0, "uri");
                properties["uri"] = json!({"type":"string","minLength":1,"maxLength":4096});
            }
            Kind::ResourceSet => {
                required.insert(0, "uris");
                properties["uris"] = json!({"type":"array","minItems":1,"maxItems":MAX_RESOURCE_URIS,
                    "uniqueItems":true,"items":{"type":"string","minLength":1,"maxLength":4096}});
            }
            Kind::Catalog(_) => {}
        }
        ToolSpec {
            name: format!("mcp_{}_{}", self.server, self.kind.label()),
            description: self.kind.description().into(),
            effect: ToolEffect::External,
            input_schema: json!({
                "type":"object","required":required,"additionalProperties":false,
                "properties":properties
            }),
        }
    }

    fn validate(&self, arguments: &Value) -> Result<(), String> {
        self.args(arguments).map(|_| ())
    }

    fn execute(
        &self,
        _: &ToolContext<'_>,
        call: &ToolCall,
        cancellation: &CancellationToken,
    ) -> Result<ToolOutput, ToolError> {
        let (target, wait_ms) = self.args(&call.arguments)?;
        let result = if self.modern {
            let watched = target.clone();
            let (status, updated_uri) = self.host.request(cancellation, move |service| {
                Box::pin(async move {
                    let filter = watched.filter();
                    let mut watch = service
                        .listen_with_capacity(filter.clone(), CAPACITY)
                        .await?;
                    let outcome = if filter.is_subset_of(watch.acknowledged()) {
                        match tokio::time::timeout(Duration::from_millis(wait_ms), watch.next())
                            .await
                        {
                            Err(_) => ("elapsed", None),
                            Ok(Ok(Some(notification))) if watched.matches(&notification) => {
                                let updated_uri = match notification {
                                    ServerNotification::ResourceUpdatedNotification(update) => {
                                        Some(update.params.uri)
                                    }
                                    _ => None,
                                };
                                ("updated", updated_uri)
                            }
                            Ok(Ok(None))
                                if matches!(watch.end(), Some(SubscriptionEnd::Graceful(_))) =>
                            {
                                ("ended", None)
                            }
                            Ok(Err(error)) => return Err(error),
                            _ => return Ok(None),
                        }
                    } else {
                        ("declined", None)
                    };
                    watch.cancel().await?;
                    Ok(Some(outcome))
                })
            })?;
            target.result(status, updated_uri)
        } else if let Target::ResourceUri(uri) = &target {
            let watched_uri = uri.clone();
            let status = self.host.request(cancellation, move |service| {
                Box::pin(legacy_watch(
                    service,
                    watched_uri,
                    Duration::from_millis(wait_ms),
                ))
            })?;
            target.result(status, None)
        } else {
            return Err("unsupported legacy MCP watch target".to_owned().into());
        };
        Ok(ToolOutput {
            output: json!({"serverId":self.server,"method":self.method(),
                "result":result,"grantsPermissions":false}),
            artifacts: Vec::new(),
        })
    }
}

/// Resource notifications on legacy sessions have no subscription ID. The
/// host serializes watches, and this observation is only a hint to read again.
#[allow(deprecated)]
async fn legacy_watch(
    service: &crate::host::McpService,
    uri: String,
    wait: Duration,
) -> Result<Option<&'static str>, ServiceError> {
    let mut updates = service.service().legacy_resource_updates.subscribe();
    match service.subscribe(SubscribeRequestParams::new(&uri)).await {
        Ok(()) => {}
        Err(ServiceError::McpError(_)) => return Ok(Some("declined")),
        Err(error) => return Err(error),
    }
    let notification = wait_for_legacy_update(&mut updates, &uri, wait).await;
    service
        .unsubscribe(UnsubscribeRequestParams::new(uri))
        .await?;
    Ok(match notification {
        Ok(true) => Some("updated"),
        Ok(false) => Some("elapsed"),
        Err(_) => None,
    })
}

async fn wait_for_legacy_update(
    updates: &mut tokio::sync::broadcast::Receiver<String>,
    uri: &str,
    wait: Duration,
) -> Result<bool, tokio::sync::broadcast::error::RecvError> {
    tokio::time::timeout(wait, async {
        loop {
            if updates.recv().await? == uri {
                return Ok(true);
            }
        }
    })
    .await
    .unwrap_or(Ok(false))
}
