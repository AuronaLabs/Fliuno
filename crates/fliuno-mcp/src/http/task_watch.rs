//! The SDK's subscription filter does not yet preserve the Tasks extension.
//! Keep this compatibility path on the same endpoint and HTTP client as MCP.
use futures_util::StreamExt;
use reqwest::{Client, Url, header};
use rmcp::model::ProtocolVersion;
use serde_json::{Value, json};
use std::sync::Arc;
use tokio::time::{Instant, timeout_at};

use crate::tasks::watch::{ACK, MAX_FRAMES, METHOD, SUBSCRIPTION_ID, accept_filter, read_status};

#[derive(Clone)]
pub(crate) struct Endpoint {
    pub(super) client: Client,
    pub(super) grant: Option<Arc<crate::oauth::OAuthGrant>>,
    pub(super) url: Url,
}

impl Endpoint {
    pub(crate) async fn observe(
        &self,
        version: &ProtocolVersion,
        task_id: &str,
        created_at: &str,
        deadline: Instant,
    ) -> &'static str {
        timeout_at(deadline, self.listen(version, task_id, created_at))
            .await
            .unwrap_or("elapsed")
    }

    async fn listen(
        &self,
        version: &ProtocolVersion,
        task_id: &str,
        created_at: &str,
    ) -> &'static str {
        let id = uuid::Uuid::new_v4().to_string();
        let config = crate::client_config().config;
        let body = json!({"jsonrpc":"2.0","id":id,"method":METHOD,"params":{
            "_meta": {
                "io.modelcontextprotocol/protocolVersion": version,
                "io.modelcontextprotocol/clientInfo": config.client_info,
                "io.modelcontextprotocol/clientCapabilities": config.capabilities,
            },
            "notifications":{"taskIds":[task_id]}
        }});
        let mut request = self
            .client
            .post(self.url.clone())
            .header("Mcp-Protocol-Version", version.as_str())
            .header("Mcp-Method", METHOD)
            .header(header::ACCEPT, "application/json, text/event-stream")
            .json(&body);
        if let Some(grant) = &self.grant {
            let Ok(header) = grant.authorization(&self.url).await else {
                return "rejected";
            };
            request = request.header(header::AUTHORIZATION, header);
        }
        let Ok(response) = request.send().await else {
            return "disconnected";
        };
        if !response.status().is_success() {
            return "rejected";
        }
        let media = response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<mime::Mime>().ok());
        if media.as_ref().map(mime::Mime::essence_str) != Some("text/event-stream") {
            return "no_stream";
        }
        let events = super::legacy_sse::bounded_events(response, 1024 * 1024);
        // reqwest closes the response stream when it is dropped. This is the
        // HTTP cancellation mechanism; no task cancellation is sent here.
        let mut messages = events.filter_map(|event| {
            std::future::ready(match event {
                Ok(event) => event
                    .data
                    .map(|data| serde_json::from_str::<Value>(&data).ok()),
                Err(_) => Some(None),
            })
        });
        let Some(Some(ack)) = messages.next().await else {
            return "disconnected";
        };
        let params = &ack["params"];
        if ack["jsonrpc"] != "2.0"
            || ack.get("id").is_some()
            || ack["method"] != ACK
            || params["_meta"][SUBSCRIPTION_ID] != id
        {
            return "invalid_stream";
        }
        if let Err(outcome) = accept_filter(params, task_id) {
            return outcome;
        }
        for _ in 0..MAX_FRAMES {
            let Some(Some(message)) = messages.next().await else {
                return "disconnected";
            };
            if message["jsonrpc"] != "2.0" {
                return "invalid_stream";
            }
            if message["id"] == id
                && message.get("method").is_none()
                && message["result"]["resultType"] == "complete"
                && message["result"]["_meta"][SUBSCRIPTION_ID] == id
            {
                return "ended";
            }
            let params = &message["params"];
            if message.get("id").is_some() || params["_meta"][SUBSCRIPTION_ID] != id {
                return "invalid_stream";
            }
            if let Err(outcome) = read_status(
                message["method"].as_str().unwrap_or_default(),
                params,
                task_id,
                created_at,
            ) {
                return outcome;
            }
        }
        "notification_limit"
    }
}
