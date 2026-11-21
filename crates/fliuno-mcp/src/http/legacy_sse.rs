//! Deprecated MCP 2024-11-05 transport: one receive stream and a separate
//! POST endpoint. A lost stream ends the connection; no reconnect or replay.
use crate::host::McpService;
use futures_util::{StreamExt, stream};
use reqwest::{Client, StatusCode, Url, header};
use rmcp::{
    ClientLifecycleMode, ClientServiceExt, RoleClient,
    model::ProtocolVersion,
    service::{RxJsonRpcMessage, TxJsonRpcMessage},
    transport::{Transport, common::client_side_sse::BoxedSseResponse},
};
use sse_stream::SseStream;
use std::{future::Future, io};

const MAX_EVENT_BYTES: usize = 1024 * 1024;
const MAX_ENDPOINT_BYTES: usize = 4096;

pub(super) async fn connect(client: Client, url: Url) -> Result<McpService, String> {
    let response = client
        .get(url.clone())
        .header(header::ACCEPT, "text/event-stream")
        .send()
        .await
        .map_err(|_| "legacy MCP SSE stream could not be opened")?;
    let content_type = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<mime::Mime>().ok());
    if response.status() != StatusCode::OK
        || content_type.as_ref().map(mime::Mime::essence_str) != Some("text/event-stream")
    {
        return Err("legacy MCP server did not open an SSE stream".into());
    }
    let mut events = bounded_events(response, MAX_EVENT_BYTES);
    let endpoint = loop {
        let missing_endpoint = "legacy MCP SSE stream did not declare a POST endpoint";
        let event = events
            .next()
            .await
            .and_then(Result::ok)
            .ok_or(missing_endpoint)?;
        if event.event.as_deref() == Some("endpoint") {
            break event.data.ok_or(missing_endpoint)?;
        }
        if event.data.is_some() || event.event.is_some() {
            return Err(missing_endpoint.into());
        }
    };
    let endpoint = resolve_endpoint(&url, &endpoint)?;
    let mut config = crate::client_config();
    config.config.capabilities.extensions = None;
    config
        .with_protocol_version(ProtocolVersion::V_2024_11_05)
        .serve_with_lifecycle(
            LegacySse {
                client,
                endpoint,
                events: Some(events),
            },
            ClientLifecycleMode::Initialize,
        )
        .await
        .map_err(|_| "legacy MCP HTTP+SSE initialization failed".into())
}

pub(super) fn bounded_events(response: reqwest::Response, limit: usize) -> BoxedSseResponse {
    let chunks = stream::try_unfold(
        (response, FrameBudget::default()),
        move |(mut response, mut budget)| async move {
            match response.chunk().await {
                Ok(Some(chunk)) => {
                    budget.observe(&chunk, limit)?;
                    Ok(Some((chunk, (response, budget))))
                }
                Ok(None) => Ok(None),
                Err(_) => Err(io::Error::other("MCP SSE stream closed")),
            }
        },
    );
    SseStream::from_bytes_stream(chunks).boxed()
}

fn resolve_endpoint(stream: &Url, endpoint: &str) -> Result<Url, String> {
    if endpoint.is_empty()
        || endpoint.len() > MAX_ENDPOINT_BYTES
        || endpoint
            .chars()
            .any(|c| c.is_control() || c.is_whitespace())
    {
        return Err("invalid legacy MCP POST endpoint".into());
    }
    let endpoint = stream
        .join(endpoint)
        .map_err(|_| "invalid legacy MCP POST endpoint")?;
    if endpoint.scheme() != stream.scheme()
        || endpoint.origin() != stream.origin()
        || endpoint.fragment().is_some()
        || !endpoint.username().is_empty()
        || endpoint.password().is_some()
        || endpoint.as_str().len() > MAX_ENDPOINT_BYTES
    {
        return Err(
            "legacy MCP POST endpoint must be same-origin without userinfo or fragments".into(),
        );
    }
    Ok(endpoint)
}

struct LegacySse {
    client: Client,
    endpoint: Url,
    events: Option<BoxedSseResponse>,
}

impl Transport<RoleClient> for LegacySse {
    type Error = io::Error;

    fn send(
        &mut self,
        message: TxJsonRpcMessage<RoleClient>,
    ) -> impl Future<Output = io::Result<()>> + Send + 'static {
        let request = self.client.post(self.endpoint.clone()).json(&message);
        async move {
            let response = request
                .send()
                .await
                .map_err(|_| io::Error::other("legacy MCP POST failed"))?;
            if !response.status().is_success() {
                return Err(io::Error::other("legacy MCP POST was rejected"));
            }
            Ok(())
        }
    }

    async fn receive(&mut self) -> Option<RxJsonRpcMessage<RoleClient>> {
        let events = self.events.as_mut()?;
        while let Some(Ok(event)) = events.next().await {
            match event.event.as_deref() {
                Some("endpoint") => return None,
                None | Some("message") => {
                    if let Some(data) = event.data {
                        return serde_json::from_str(&data).ok();
                    }
                }
                _ => {}
            }
        }
        None
    }

    async fn close(&mut self) -> io::Result<()> {
        self.events.take();
        Ok(())
    }
}

#[derive(Default)]
struct FrameBudget {
    bytes: usize,
    nonempty_line: bool,
    cr: bool,
}

impl FrameBudget {
    // Count raw bytes before the parser can buffer them, across HTTP chunks.
    fn observe(&mut self, chunk: &[u8], limit: usize) -> io::Result<()> {
        for &byte in chunk {
            let previous_cr = std::mem::replace(&mut self.cr, byte == b'\r');
            if previous_cr && byte == b'\n' {
                continue;
            }
            if matches!(byte, b'\n' | b'\r') {
                if !std::mem::take(&mut self.nonempty_line) {
                    self.bytes = 0;
                    continue;
                }
            } else {
                self.nonempty_line = true;
            }
            self.bytes += 1;
            if self.bytes > limit {
                return Err(io::Error::other("MCP SSE frame exceeds the byte budget"));
            }
        }
        Ok(())
    }
}
