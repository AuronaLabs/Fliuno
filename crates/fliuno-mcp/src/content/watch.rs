//! An approved watch observes at most one notification from one subscription.
use super::{Host, Tool, ToolContext, ToolError, ToolOutput, ToolSpec, arguments, authority};
use fliuno_core::{CancellationToken, ToolEffect};
use fliuno_protocol::ToolCall;
use rmcp::{
    model::{ServerCapabilities, ServerNotification, SubscriptionFilter},
    service::SubscriptionEnd,
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{num::NonZeroUsize, sync::Arc, time::Duration};

const METHOD: &str = "subscriptions/listen";
const CAPACITY: NonZeroUsize = NonZeroUsize::new(8).unwrap();
const MAX_WAIT_MS: u64 = 5_000;

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

#[derive(Clone, Copy)]
enum Kind {
    ResourceUri,
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
    Catalog(Catalog),
}

impl Target {
    fn filter(&self) -> SubscriptionFilter {
        let builder = SubscriptionFilter::builder();
        match self {
            Self::ResourceUri(uri) => builder.resource_subscription(uri),
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

    fn result(&self, status: &str) -> Value {
        match self {
            Self::ResourceUri(uri) => json!({"uri":uri,"status":status}),
            Self::Catalog(_) => json!({"status":status}),
        }
    }
}

pub(super) fn register(
    host: Arc<Host>,
    server: &str,
    caps: &ServerCapabilities,
) -> Vec<Box<dyn Tool>> {
    // Keep part of the call deadline for opening and closing the subscription.
    let max_wait_ms = MAX_WAIT_MS.min(host.timeout.as_millis() as u64 / 2);
    [
        (
            Kind::ResourceUri,
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
    .filter(|(_, enabled)| *enabled == Some(true))
    .map(|(kind, _)| {
        Box::new(Watch {
            host: Arc::clone(&host),
            kind,
            server: server.into(),
            max_wait_ms,
        }) as Box<dyn Tool>
    })
    .collect()
}

struct Watch {
    host: Arc<Host>,
    kind: Kind,
    server: String,
    max_wait_ms: u64,
}

impl Watch {
    fn args(&self, value: &Value) -> Result<(Target, u64), String> {
        let invalid = || match self.kind {
            Kind::ResourceUri => "invalid MCP watch URI or waitMs".to_owned(),
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
        authority(&self.host, &self.spec(), METHOD)
    }

    fn spec(&self) -> ToolSpec {
        let mut required = vec!["waitMs"];
        let mut properties =
            json!({"waitMs":{"type":"integer","minimum":1,"maximum":self.max_wait_ms}});
        if matches!(self.kind, Kind::ResourceUri) {
            required.insert(0, "uri");
            properties["uri"] = json!({"type":"string","minLength":1,"maxLength":4096});
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
        let watched = target.clone();
        let status = self.host.request(cancellation, move |service| {
            Box::pin(async move {
                let filter = watched.filter();
                let mut watch = service
                    .listen_with_capacity(filter.clone(), CAPACITY)
                    .await?;
                let status = if filter.is_subset_of(watch.acknowledged()) {
                    match tokio::time::timeout(Duration::from_millis(wait_ms), watch.next()).await {
                        Err(_) => "elapsed",
                        Ok(Ok(Some(notification))) if watched.matches(&notification) => "updated",
                        Ok(Ok(None))
                            if matches!(watch.end(), Some(SubscriptionEnd::Graceful(_))) =>
                        {
                            "ended"
                        }
                        Ok(Err(error)) => return Err(error),
                        _ => return Ok(None),
                    }
                } else {
                    "declined"
                };
                watch.cancel().await?;
                Ok(Some(status))
            })
        })?;
        Ok(ToolOutput {
            output: json!({"serverId":self.server,"method":METHOD,
                "result":target.result(status),"grantsPermissions":false}),
            artifacts: Vec::new(),
        })
    }
}
