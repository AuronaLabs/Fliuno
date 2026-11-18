//! An approved watch observes at most one update to one remote URI.
use super::{Host, Tool, ToolContext, ToolError, ToolOutput, ToolSpec, arguments, authority};
use fliuno_core::{CancellationToken, ToolEffect};
use fliuno_protocol::ToolCall;
use rmcp::{
    model::{ServerNotification, SubscriptionFilter},
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
struct Args {
    uri: String,
    wait_ms: u64,
}

pub(super) struct ResourceWatch {
    host: Arc<Host>,
    server: String,
    max_wait_ms: u64,
}

impl ResourceWatch {
    pub(super) fn new(host: Arc<Host>, server: &str) -> Self {
        // Leave part of this call's transport budget for the acknowledgment
        // and cleanup. Queue time still shares the existing Host deadline.
        let max_wait_ms = MAX_WAIT_MS.min(host.timeout.as_millis() as u64 / 2);
        Self {
            host,
            server: server.into(),
            max_wait_ms,
        }
    }

    fn args(&self, value: &Value) -> Result<Args, String> {
        let invalid = || "invalid MCP watch URI or waitMs".to_owned();
        let args = Args::deserialize(value).map_err(|_| invalid())?;
        if !arguments::valid_uri(&args.uri) || !(1..=self.max_wait_ms).contains(&args.wait_ms) {
            return Err(invalid());
        }
        Ok(args)
    }
}

impl Tool for ResourceWatch {
    fn authority(&self) -> String {
        authority(&self.host, &self.spec(), METHOD)
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: format!("mcp_{}_resources_watch", self.server),
            description: "Watch one remote resource URI for one update during the approved wait. The notification contains no fetched resource body.".into(),
            effect: ToolEffect::External,
            input_schema: json!({
                "type":"object","required":["uri","waitMs"],"additionalProperties":false,
                "properties": {
                    "uri":{"type":"string","minLength":1,"maxLength":4096},
                    "waitMs":{"type":"integer","minimum":1,"maximum":self.max_wait_ms}
                }
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
        let Args { uri, wait_ms } = self.args(&call.arguments)?;
        let watched_uri = uri.clone();
        let status = self.host.request(cancellation, move |service| {
            Box::pin(async move {
                let filter = SubscriptionFilter::builder()
                    .resource_subscription(&watched_uri)
                    .build();
                let mut watch = service.listen_with_capacity(filter, CAPACITY).await?;
                let accepted = watch
                    .acknowledged()
                    .resource_subscriptions
                    .as_ref()
                    .is_some_and(|uris| uris.iter().any(|candidate| candidate == &watched_uri));
                let status = if accepted {
                    match tokio::time::timeout(Duration::from_millis(wait_ms), watch.next()).await {
                        Err(_) => "elapsed",
                        Ok(Ok(Some(ServerNotification::ResourceUpdatedNotification(update))))
                            if update.params.uri == watched_uri =>
                        {
                            "updated"
                        }
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
                "result":{"uri":uri,"status":status},"grantsPermissions":false}),
            artifacts: Vec::new(),
        })
    }
}
