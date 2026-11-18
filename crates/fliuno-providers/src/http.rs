use crate::ModelContextBudget;
use fliuno_core::{CancellationToken, ModelError};
use reqwest::{
    Client, Url,
    header::{CONTENT_TYPE, HeaderMap},
    redirect::Policy,
};
use serde_json::Value;
use std::{collections::BTreeMap, time::Duration};
use tokio::runtime::Runtime;

const REQUEST_LIMIT: usize = 8 * 1024 * 1024;
const RESPONSE_LIMIT: u64 = 4 * 1024 * 1024;
const EVENT_LIMIT: usize = 1024 * 1024;
const COUNT_RESPONSE_LIMIT: u64 = 16 * 1024;

struct BoundedRequestBody {
    bytes: Vec<u8>,
    limit: usize,
    exceeded: bool,
}

impl std::io::Write for BoundedRequestBody {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        if buffer.len() > self.limit.saturating_sub(self.bytes.len()) {
            self.exceeded = true;
            return Err(std::io::Error::other(
                "provider request body limit exceeded",
            ));
        }
        self.bytes.extend_from_slice(buffer);
        Ok(buffer.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn encode_request(
    body: &impl serde::Serialize,
    budget: Option<ModelContextBudget>,
) -> Result<Vec<u8>, ModelError> {
    let limit = budget
        .filter(|budget| !budget.provider_count)
        .map_or(REQUEST_LIMIT, |budget| {
            REQUEST_LIMIT.min(budget.input_allowance as usize)
        });
    let mut writer = BoundedRequestBody {
        bytes: Vec::new(),
        limit,
        exceeded: false,
    };
    if serde_json::to_writer(&mut writer, body).is_err() {
        let message = match (writer.exceeded, limit < REQUEST_LIMIT) {
            (true, true) => {
                "provider input exceeds the configured local context estimate (JSON bytes plus output reserve)"
            }
            (true, false) => "provider request exceeds 8 MiB",
            (false, _) => "cannot serialize provider request",
        };
        return Err(ModelError::NotDispatched(message.into()));
    }
    Ok(writer.bytes)
}

pub(crate) struct Http {
    client: Client,
    base: Url,
    endpoint: Url,
    count_endpoint: Url,
    count_fields: &'static [&'static str],
    runtime: Runtime,
    context_budget: Option<ModelContextBudget>,
}

impl Http {
    pub fn new(
        base: &str,
        route: &str,
        count_route: &str,
        count_fields: &'static [&'static str],
        headers: HeaderMap,
    ) -> Result<Self, String> {
        let mut base = Url::parse(base).map_err(|_| "invalid provider base URL")?;
        if !base.username().is_empty()
            || base.password().is_some()
            || base.query().is_some()
            || base.fragment().is_some()
        {
            return Err("provider base URL cannot contain credentials, query or fragment".into());
        }
        let local = matches!(base.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"));
        if base.scheme() != "https" && !(base.scheme() == "http" && local) {
            return Err("provider requires HTTPS (HTTP is allowed only on loopback)".into());
        }
        if !base.path().ends_with('/') {
            base.set_path(&format!("{}/", base.path()));
        }
        let endpoint = base.join(route).map_err(|_| "invalid provider route")?;
        let count_endpoint = base
            .join(count_route)
            .map_err(|_| "invalid provider count route")?;
        let client = Client::builder()
            .default_headers(headers)
            .timeout(Duration::from_secs(45))
            .connect_timeout(Duration::from_secs(10))
            .redirect(Policy::none())
            .retry(reqwest::retry::never())
            .build()
            .map_err(|_| "cannot initialize HTTP client")?;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|_| "cannot initialize provider runtime")?;
        Ok(Self {
            client,
            base,
            endpoint,
            count_endpoint,
            count_fields,
            runtime,
            context_budget: None,
        })
    }

    pub fn with_context_budget(mut self, budget: ModelContextBudget) -> Self {
        self.context_budget = Some(budget);
        self
    }

    pub fn max_output_tokens(&self) -> Option<u32> {
        self.context_budget.map(|budget| budget.max_output_tokens)
    }

    pub fn configured_budget(&self) -> Option<fliuno_protocol::ModelBudget> {
        self.context_budget.map(ModelContextBudget::describe)
    }

    pub fn model_metadata(&self, model: &str) -> Result<Value, String> {
        let mut endpoint = self.base.clone();
        endpoint
            .path_segments_mut()
            .map_err(|_| "invalid provider metadata URL")?
            .pop_if_empty()
            .push("models")
            .push(model);
        let cancellation = CancellationToken::new();
        self.runtime
            .block_on(async {
                tokio::time::timeout(Duration::from_secs(5), async {
                    let response = self
                        .client
                        .get(endpoint)
                        .header(reqwest::header::ACCEPT, "application/json")
                        .send()
                        .await
                        .map_err(|_| "provider metadata transport failed")?;
                    Self::read_json(response, &cancellation, 64 * 1024)
                        .await
                        .map_err(|_| "provider metadata response was unsuccessful or invalid")
                })
                .await
            })
            .map_err(|_| "provider metadata request timed out")?
            .map_err(str::to_owned)
    }

    fn prepare_generation(
        &self,
        body: &Value,
        cancellation: &CancellationToken,
    ) -> Result<Vec<u8>, ModelError> {
        if cancellation.is_cancelled() {
            return Err(ModelError::CancelledBeforeDispatch);
        }
        let encoded = encode_request(body, self.context_budget)?;
        if let Some(budget) = self.context_budget.filter(|budget| budget.provider_count) {
            let count_body: BTreeMap<_, _> = self
                .count_fields
                .iter()
                .copied()
                .filter_map(|key| body.get(key).map(|value| (key, value)))
                .collect();
            let count_body = encode_request(&count_body, None)?;
            let count = self.runtime.block_on(async {
                tokio::select! {
                    biased;
                    _ = cancellation.cancelled() => Err(ModelError::CancelledBeforeDispatch),
                    result = tokio::time::timeout(Duration::from_secs(10), self.post_json(&self.count_endpoint, count_body, cancellation, COUNT_RESPONSE_LIMIT)) => {
                        let value = result.ok().and_then(Result::ok);
                        value.as_ref().and_then(|value| value.get("input_tokens"))
                            .and_then(Value::as_u64)
                            .ok_or_else(|| ModelError::NotDispatched("provider input token count unavailable; generation not sent".into()))
                    }
                }
            });
            if cancellation.is_cancelled() {
                return Err(ModelError::CancelledBeforeDispatch);
            }
            if count? > u64::from(budget.input_allowance) {
                return Err(ModelError::NotDispatched(
                    "provider input token count exceeds the configured context budget".into(),
                ));
            }
        }
        if cancellation.is_cancelled() {
            return Err(ModelError::CancelledBeforeDispatch);
        }
        Ok(encoded)
    }

    pub fn post(
        &self,
        body: &Value,
        cancellation: &CancellationToken,
    ) -> Result<Value, ModelError> {
        let body = self.prepare_generation(body, cancellation)?;
        self.runtime
            .block_on(self.post_json(&self.endpoint, body, cancellation, RESPONSE_LIMIT))
    }

    pub fn post_stream(
        &self,
        body: &Value,
        cancellation: &CancellationToken,
        on_event: impl FnMut(Value) -> Result<(), String>,
    ) -> Result<Option<Value>, ModelError> {
        let body = self.prepare_generation(body, cancellation)?;
        self.runtime
            .block_on(self.post_stream_async(body, cancellation, on_event))
    }

    async fn post_json(
        &self,
        endpoint: &Url,
        body: Vec<u8>,
        cancellation: &CancellationToken,
        limit: u64,
    ) -> Result<Value, ModelError> {
        let response = tokio::select! {
            _ = cancellation.cancelled() => return Err("provider request cancelled".into()),
            response = self.client.post(endpoint.clone())
                .header(CONTENT_TYPE, "application/json")
                .body(body)
                .send() => {
                response.map_err(|error| ModelError::OutcomeUnknown(
                    if error.is_timeout() { "provider timed out" } else { "provider transport failed" }.into()
                ))?
            }
        };
        Self::read_json(response, cancellation, limit).await
    }

    async fn read_json(
        mut response: reqwest::Response,
        cancellation: &CancellationToken,
        limit: u64,
    ) -> Result<Value, ModelError> {
        let status = response.status();
        // Do not persist remote error bodies: they may echo credentials or prompts.
        if !status.is_success() {
            return Err(format!("provider HTTP status {}", status.as_u16()).into());
        }
        if response
            .content_length()
            .is_some_and(|length| length > limit)
        {
            return Err("provider response exceeds its size limit".into());
        }
        let mut bytes = Vec::new();
        loop {
            let chunk = tokio::select! {
                _ = cancellation.cancelled() => return Err("provider request cancelled".into()),
                chunk = response.chunk() => chunk.map_err(|_| "cannot read provider response")?,
            };
            let Some(chunk) = chunk else { break };
            if bytes.len().saturating_add(chunk.len()) as u64 > limit {
                return Err("provider response exceeds its size limit".into());
            }
            bytes.extend_from_slice(&chunk);
        }
        serde_json::from_slice(&bytes).map_err(|_| "provider returned invalid JSON".into())
    }

    async fn post_stream_async(
        &self,
        body: Vec<u8>,
        cancellation: &CancellationToken,
        mut on_event: impl FnMut(Value) -> Result<(), String>,
    ) -> Result<Option<Value>, ModelError> {
        let response = tokio::select! {
            _ = cancellation.cancelled() => return Err("provider request cancelled".into()),
            response = self.client.post(self.endpoint.clone())
                .header(reqwest::header::ACCEPT, "text/event-stream")
                .header(CONTENT_TYPE, "application/json")
                .body(body)
                .send() => {
                response.map_err(|error| ModelError::OutcomeUnknown(
                    if error.is_timeout() { "provider timed out" } else { "provider transport failed" }.into()
                ))?
            }
        };
        let status = response.status();
        if !status.is_success() {
            return Err(format!("provider HTTP status {}", status.as_u16()).into());
        }
        let media_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.split(';').next())
            .map(str::trim)
            .unwrap_or("")
            .to_owned();
        let mut response = response;
        if media_type.eq_ignore_ascii_case("application/json")
            || media_type.to_ascii_lowercase().ends_with("+json")
        {
            let mut bytes = Vec::new();
            loop {
                let chunk = tokio::select! {
                    _ = cancellation.cancelled() => return Err("provider request cancelled".into()),
                    chunk = response.chunk() => chunk.map_err(|_| "cannot read provider response")?,
                };
                let Some(chunk) = chunk else { break };
                if bytes.len().saturating_add(chunk.len()) as u64 > RESPONSE_LIMIT {
                    return Err("provider response exceeds 4 MiB".into());
                }
                bytes.extend_from_slice(&chunk);
            }
            let response = serde_json::from_slice(&bytes)
                .map_err(|_| "provider returned invalid JSON".to_owned())?;
            // A compatibility JSON response is distinct from an SSE event;
            // an event cannot masquerade as a complete response by adding fields.
            return Ok(Some(response));
        }
        if !media_type.eq_ignore_ascii_case("text/event-stream") {
            return Err("provider returned an unsupported response content type".into());
        }
        let mut decoder = SseDecoder::default();
        loop {
            let chunk = tokio::select! {
                _ = cancellation.cancelled() => return Err("provider request cancelled".into()),
                chunk = response.chunk() => chunk.map_err(|_| "cannot read provider event stream")?,
            };
            let Some(chunk) = chunk else { break };
            decoder.feed(&chunk, &mut on_event)?;
        }
        decoder.finish()?;
        Ok(None)
    }
}

#[derive(Default)]
struct SseDecoder {
    line: Vec<u8>,
    data: Vec<u8>,
    bytes_seen: u64,
    previous_cr: bool,
    saw_first_line: bool,
}

impl SseDecoder {
    fn feed(
        &mut self,
        bytes: &[u8],
        on_event: &mut impl FnMut(Value) -> Result<(), String>,
    ) -> Result<(), String> {
        self.bytes_seen = self.bytes_seen.saturating_add(bytes.len() as u64);
        if self.bytes_seen > RESPONSE_LIMIT {
            return Err("provider response exceeds 4 MiB".into());
        }
        for byte in bytes {
            if self.previous_cr {
                self.previous_cr = false;
                if *byte == b'\n' {
                    continue;
                }
            }
            if matches!(byte, b'\r' | b'\n') {
                self.process_line(on_event)?;
                self.previous_cr = *byte == b'\r';
            } else {
                self.line.push(*byte);
                if self.line.len() > EVENT_LIMIT {
                    return Err("provider event exceeds 1 MiB".into());
                }
            }
        }
        Ok(())
    }

    fn finish(&self) -> Result<(), String> {
        // EOF is not an SSE event delimiter. A truncated completion frame must
        // never turn an uncertain response into an authoritative model result.
        if !self.line.is_empty() || !self.data.is_empty() {
            return Err("provider event stream ended within an event".into());
        }
        Ok(())
    }

    fn process_line(
        &mut self,
        on_event: &mut impl FnMut(Value) -> Result<(), String>,
    ) -> Result<(), String> {
        if !self.saw_first_line {
            self.saw_first_line = true;
            if self.line.starts_with(&[0xef, 0xbb, 0xbf]) {
                self.line.drain(..3);
            }
        }
        if self.line.is_empty() {
            return self.dispatch(on_event);
        }
        if self.line.first() == Some(&b':') {
            self.line.clear();
            return Ok(());
        }
        if let Some(value) = self.line.strip_prefix(b"data:") {
            let value = value.strip_prefix(b" ").unwrap_or(value);
            self.data.extend_from_slice(value);
            self.data.push(b'\n');
            if self.data.len() > EVENT_LIMIT {
                return Err("provider event exceeds 1 MiB".into());
            }
        }
        self.line.clear();
        Ok(())
    }

    fn dispatch(
        &mut self,
        on_event: &mut impl FnMut(Value) -> Result<(), String>,
    ) -> Result<(), String> {
        if self.data.is_empty() {
            return Ok(());
        }
        if self.data.last() == Some(&b'\n') {
            self.data.pop();
        }
        if self.data == b"[DONE]" {
            self.data.clear();
            return Ok(());
        }
        let event = serde_json::from_slice(&self.data)
            .map_err(|_| "provider returned invalid event JSON")?;
        self.data.clear();
        on_event(event)
    }
}

pub(crate) fn header(value: &str) -> Result<reqwest::header::HeaderValue, String> {
    let mut header =
        reqwest::header::HeaderValue::from_str(value).map_err(|_| "invalid API credential")?;
    header.set_sensitive(true);
    Ok(header)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn serialized_provider_request_accepts_the_exact_limit_and_rejects_one_more_byte() {
        let exact = Value::String("x".repeat(REQUEST_LIMIT - 2));
        let encoded = encode_request(&exact, None).unwrap();
        assert_eq!(encoded.len(), REQUEST_LIMIT);

        let over = Value::String("x".repeat(REQUEST_LIMIT - 1));
        assert!(matches!(
            encode_request(&over, None),
            Err(ModelError::NotDispatched(message)) if message == "provider request exceeds 8 MiB"
        ));
    }

    #[test]
    fn sse_decoder_handles_fragmented_utf8_and_crlf_frames() {
        let bytes = "data: {\"type\":\"delta\",\r\ndata: \"text\":\"你好\"}\r\n\r\n".as_bytes();
        let mut decoder = SseDecoder::default();
        let mut events = Vec::new();
        for chunk in bytes.chunks(3) {
            decoder
                .feed(chunk, &mut |event| {
                    events.push(event);
                    Ok(())
                })
                .unwrap();
        }
        decoder.finish().unwrap();
        assert_eq!(events, vec![json!({"type":"delta","text":"你好"})]);
    }

    #[test]
    fn sse_decoder_rejects_oversized_events_and_invalid_json() {
        let mut decoder = SseDecoder::default();
        let oversized = format!("data: {}\n", "x".repeat(EVENT_LIMIT));
        assert!(decoder.feed(oversized.as_bytes(), &mut |_| Ok(())).is_err());

        let mut decoder = SseDecoder::default();
        decoder
            .feed(b"data: not-json\n\n", &mut |_| Ok(()))
            .unwrap_err();
    }

    #[test]
    fn sse_decoder_does_not_dispatch_an_unterminated_eof_frame() {
        let mut decoder = SseDecoder::default();
        let mut events = Vec::new();
        decoder
            .feed(b"data: {\"type\":\"response.completed\"}\n", &mut |event| {
                events.push(event);
                Ok(())
            })
            .unwrap();
        assert!(decoder.finish().is_err());
        assert!(events.is_empty());
    }
}
