use crate::ModelContextBudget;
use fliuno_core::{CancellationToken, InputBudgetError, ModelError};
use fliuno_protocol::{ModelBudget, ModelBudgetLimit, ModelBudgetRejection, ModelInputPreflight};
use reqwest::{
    Client, Url,
    header::{CONTENT_TYPE, HeaderMap},
    redirect::Policy,
};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    sync::atomic::{AtomicU32, Ordering},
    time::Duration,
};
use tokio::runtime::Runtime;

const REQUEST_LIMIT: usize = 8 * 1024 * 1024;
const RESPONSE_LIMIT: u64 = 4 * 1024 * 1024;
const EVENT_LIMIT: usize = 1024 * 1024;
const COUNT_RESPONSE_LIMIT: u64 = 16 * 1024;

struct BoundedRequestBody {
    bytes: Vec<u8>,
    limit: usize,
    exceeded_at: Option<usize>,
}

impl std::io::Write for BoundedRequestBody {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        if buffer.len() > self.limit.saturating_sub(self.bytes.len()) {
            self.exceeded_at = Some(self.bytes.len().saturating_add(buffer.len()));
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
    budget: Option<ModelBudget>,
    count_for_generation_bytes: Option<usize>,
) -> Result<Vec<u8>, ModelError> {
    let limit = if count_for_generation_bytes.is_some() {
        REQUEST_LIMIT
    } else {
        budget
            .and_then(|budget| budget.max_input_bytes)
            .map_or(REQUEST_LIMIT, |limit| REQUEST_LIMIT.min(limit as usize))
    };
    let mut writer = BoundedRequestBody {
        bytes: Vec::new(),
        limit,
        exceeded_at: None,
    };
    if serde_json::to_writer(&mut writer, body).is_err() {
        let Some(observed) = writer.exceeded_at else {
            return Err(ModelError::NotDispatched(
                "cannot serialize provider request".into(),
            ));
        };
        let (kind, message) = if count_for_generation_bytes.is_some() {
            (
                ModelBudgetLimit::CountRequestBytes,
                "provider count request exceeds 8 MiB",
            )
        } else if limit < REQUEST_LIMIT {
            (
                ModelBudgetLimit::LocalJsonBytes,
                "provider input exceeds the configured local context estimate (JSON bytes plus output reserve)",
            )
        } else {
            (
                ModelBudgetLimit::GenerationRequestBytes,
                "provider request exceeds 8 MiB",
            )
        };
        return Err(ModelError::InputBudgetExceeded(InputBudgetError {
            message: message.into(),
            report: Some(Box::new(ModelBudgetRejection {
                kind,
                limit: limit as u64,
                observed: observed as u64,
                observed_is_lower_bound: true,
                transport_limit_bytes: REQUEST_LIMIT as u64,
                serialized_generation_bytes: count_for_generation_bytes.map(|bytes| bytes as u64),
                budget,
                normalized_input_bytes: None,
            })),
        }));
    }
    Ok(writer.bytes)
}

pub(crate) struct StreamResponse {
    pub json: Option<Value>,
    pub done: bool,
}

pub(crate) struct Http {
    client: Client,
    base: Url,
    endpoint: Url,
    count_endpoint: Url,
    count_fields: &'static [&'static str],
    count_tokens_field: &'static str,
    runtime: Runtime,
    context_budget: Option<ModelContextBudget>,
    calibrated_safety_margin_tokens: AtomicU32,
}

impl Http {
    pub fn new(
        base: &str,
        route: &str,
        count_route: &str,
        count_fields: &'static [&'static str],
        headers: HeaderMap,
    ) -> Result<Self, String> {
        let base = fliuno_config::provider_api_root(base)?;
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
            count_tokens_field: "input_tokens",
            runtime,
            context_budget: None,
            calibrated_safety_margin_tokens: AtomicU32::new(0),
        })
    }

    pub fn with_count_tokens_field(mut self, field: &'static str) -> Self {
        self.count_tokens_field = field;
        self
    }

    pub fn with_count_model_route(mut self, model: &str) -> Result<Self, String> {
        let model = model.strip_prefix("models/").unwrap_or(model);
        if model.is_empty()
            || model.len() > 256
            || !model
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
        {
            return Err("invalid model identifier for provider token counting".into());
        }
        let mut endpoint = self.base.clone();
        endpoint
            .path_segments_mut()
            .map_err(|_| "invalid provider count URL")?
            .pop_if_empty()
            .push("models")
            .push(&format!("{model}:countTokens"));
        self.count_endpoint = endpoint;
        Ok(self)
    }

    pub fn uses_provider_count(&self) -> bool {
        self.context_budget
            .is_some_and(|budget| budget.provider_count)
    }

    pub fn with_context_budget(mut self, budget: ModelContextBudget) -> Self {
        self.context_budget = Some(budget);
        self
    }

    pub fn max_output_tokens(&self) -> Option<u32> {
        self.context_budget.map(|budget| budget.max_output_tokens)
    }

    pub fn configured_budget(&self) -> Option<fliuno_protocol::ModelBudget> {
        self.context_budget.map(|budget| {
            let mut description = budget.describe();
            description.safety_margin_tokens = description
                .safety_margin_tokens
                .max(self.calibrated_safety_margin_tokens.load(Ordering::Relaxed));
            description
        })
    }

    pub fn calibrate_safety_margin(&self, tokens: u32) {
        self.calibrated_safety_margin_tokens
            .fetch_max(tokens, Ordering::Relaxed);
    }

    pub fn observe_preflight(&self, estimate: u64, actual: u64) {
        let learned = u32::try_from(actual.saturating_sub(estimate)).unwrap_or(u32::MAX);
        self.calibrate_safety_margin(learned);
    }

    pub fn preflight_report(
        &self,
        provider: &str,
        model: &str,
        estimated_tokens: Option<u64>,
    ) -> Option<ModelInputPreflight> {
        let budget = self.configured_budget()?;
        if budget.count_mode != fliuno_protocol::ModelInputCountMode::Provider {
            return None;
        }
        Some(ModelInputPreflight {
            provider: provider.into(),
            model: model.into(),
            count_coverage: budget.count_coverage,
            estimated_input_tokens: estimated_tokens?,
            safety_margin_tokens: budget.safety_margin_tokens,
        })
    }

    pub fn model_metadata(&self, model: &str) -> Result<Value, String> {
        let mut endpoint = self.base.clone();
        endpoint
            .path_segments_mut()
            .map_err(|_| "invalid provider metadata URL")?
            .pop_if_empty()
            .push("models")
            .push(model);
        self.read_metadata(endpoint, 64 * 1024, |_| {
            "provider metadata response was unsuccessful or invalid".into()
        })
    }

    pub fn model_catalog(&self, params: &[(&str, &str)]) -> Result<Value, String> {
        let mut endpoint = self.endpoint.clone();
        if !params.is_empty() {
            endpoint
                .query_pairs_mut()
                .extend_pairs(params.iter().copied());
        }
        self.read_metadata(endpoint, 2 * 1024 * 1024, |error| error.to_string())
    }

    fn read_metadata(
        &self,
        endpoint: Url,
        limit: u64,
        response_error: impl FnOnce(ModelError) -> String,
    ) -> Result<Value, String> {
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
                        .map_err(|_| "provider metadata transport failed".to_owned())?;
                    Self::read_json(response, &cancellation, limit)
                        .await
                        .map_err(response_error)
                })
                .await
            })
            .map_err(|_| "provider metadata request timed out")?
    }

    fn prepare_generation(
        &self,
        body: &Value,
        provider_count_body: Option<&Value>,
        cancellation: &CancellationToken,
    ) -> Result<(Vec<u8>, Option<u64>), ModelError> {
        if cancellation.is_cancelled() {
            return Err(ModelError::CancelledBeforeDispatch);
        }
        let configured = self.configured_budget();
        let encoded = encode_request(body, configured, None)?;
        let mut observed_count = None;
        if let Some(mut budget) = configured.filter(|budget| budget.max_input_tokens.is_some()) {
            let count_body = if let Some(count_body) = provider_count_body {
                encode_request(count_body, configured, Some(encoded.len()))?
            } else {
                let count_body: BTreeMap<_, _> = self
                    .count_fields
                    .iter()
                    .copied()
                    .filter_map(|key| body.get(key).map(|value| (key, value)))
                    .collect();
                encode_request(&count_body, configured, Some(encoded.len()))?
            };
            let count = self.runtime.block_on(async {
                tokio::select! {
                    biased;
                    _ = cancellation.cancelled() => Err(ModelError::CancelledBeforeDispatch),
                    result = tokio::time::timeout(Duration::from_secs(10), self.post_json(&self.count_endpoint, count_body, cancellation, COUNT_RESPONSE_LIMIT)) => {
                        let value = result.ok().and_then(Result::ok);
                        value.as_ref().and_then(|value| value.get(self.count_tokens_field))
                            .and_then(Value::as_u64)
                            .ok_or_else(|| ModelError::NotDispatched("provider input token count unavailable; generation not sent".into()))
                    }
                }
            });
            if cancellation.is_cancelled() {
                return Err(ModelError::CancelledBeforeDispatch);
            }
            let count = count?;
            observed_count = Some(count);
            budget.safety_margin_tokens = budget
                .safety_margin_tokens
                .max(self.calibrated_safety_margin_tokens.load(Ordering::Relaxed));
            let available = budget
                .max_input_tokens
                .unwrap()
                .saturating_sub(budget.safety_margin_tokens);
            if count > u64::from(available) {
                return Err(ModelError::InputBudgetExceeded(InputBudgetError {
                    message: "provider input token count plus its safety margin exceeds the configured context budget".into(),
                    report: Some(Box::new(ModelBudgetRejection {
                        kind: ModelBudgetLimit::ProviderInputTokens,
                        limit: u64::from(available),
                        observed: count,
                        observed_is_lower_bound: false,
                        transport_limit_bytes: REQUEST_LIMIT as u64,
                        serialized_generation_bytes: Some(encoded.len() as u64),
                        budget: Some(budget),
                        normalized_input_bytes: None,
                    })),
                }));
            }
        }
        if cancellation.is_cancelled() {
            return Err(ModelError::CancelledBeforeDispatch);
        }
        Ok((encoded, observed_count))
    }

    pub fn post_observed(
        &self,
        body: &Value,
        cancellation: &CancellationToken,
    ) -> Result<(Value, Option<u64>), ModelError> {
        let (body, estimate) = self.prepare_generation(body, None, cancellation)?;
        let response = self.runtime.block_on(self.post_json(
            &self.endpoint,
            body,
            cancellation,
            RESPONSE_LIMIT,
        ))?;
        Ok((response, estimate))
    }

    pub fn post_with_count_observed(
        &self,
        body: &Value,
        count_body: &Value,
        cancellation: &CancellationToken,
    ) -> Result<(Value, Option<u64>), ModelError> {
        let (body, estimate) = self.prepare_generation(body, Some(count_body), cancellation)?;
        let response = self.runtime.block_on(self.post_json(
            &self.endpoint,
            body,
            cancellation,
            RESPONSE_LIMIT,
        ))?;
        Ok((response, estimate))
    }

    pub fn post_stream_observed(
        &self,
        body: &Value,
        cancellation: &CancellationToken,
        on_event: impl FnMut(Value) -> Result<(), String>,
    ) -> Result<(StreamResponse, Option<u64>), ModelError> {
        let (body, estimate) = self.prepare_generation(body, None, cancellation)?;
        let response =
            self.runtime
                .block_on(self.post_stream_async(body, cancellation, on_event))?;
        Ok((response, estimate))
    }

    pub fn post_stream_with_count_observed(
        &self,
        body: &Value,
        count_body: &Value,
        cancellation: &CancellationToken,
        on_event: impl FnMut(Value) -> Result<(), String>,
    ) -> Result<(StreamResponse, Option<u64>), ModelError> {
        let (body, estimate) = self.prepare_generation(body, Some(count_body), cancellation)?;
        let response =
            self.runtime
                .block_on(self.post_stream_async(body, cancellation, on_event))?;
        Ok((response, estimate))
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
    ) -> Result<StreamResponse, ModelError> {
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
            return Ok(StreamResponse {
                json: Some(response),
                done: false,
            });
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
        Ok(StreamResponse {
            json: None,
            done: decoder.done,
        })
    }
}

#[derive(Default)]
struct SseDecoder {
    line: Vec<u8>,
    data: Vec<u8>,
    bytes_seen: u64,
    previous_cr: bool,
    saw_first_line: bool,
    done: bool,
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
        self.done = self.data == b"[DONE]";
        if self.done {
            self.data.clear();
            return Ok(());
        }
        let event = serde_json::from_slice(&self.data)
            .map_err(|_| "provider returned invalid event JSON")?;
        self.data.clear();
        on_event(event)
    }
}

pub(crate) fn provider_headers(
    kind: fliuno_config::Provider,
    key: &str,
) -> Result<HeaderMap, String> {
    use fliuno_config::Provider;
    if key.trim().is_empty() {
        return Err("API key must not be empty".into());
    }
    let mut headers = HeaderMap::new();
    match kind {
        Provider::Demo => return Err("Choose a configured live model provider".into()),
        Provider::OpenAi => {
            headers.insert(
                reqwest::header::AUTHORIZATION,
                header(&format!("Bearer {key}"))?,
            );
        }
        Provider::Anthropic => {
            headers.insert("x-api-key", header(key)?);
            headers.insert(
                "anthropic-version",
                reqwest::header::HeaderValue::from_static("2023-06-01"),
            );
        }
        Provider::Gemini => {
            headers.insert("x-goog-api-key", header(key)?);
        }
    }
    Ok(headers)
}

fn header(value: &str) -> Result<reqwest::header::HeaderValue, String> {
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
        let encoded = encode_request(&exact, None, None).unwrap();
        assert_eq!(encoded.len(), REQUEST_LIMIT);

        let over = Value::String("x".repeat(REQUEST_LIMIT - 1));
        assert!(matches!(
            encode_request(&over, None, None),
            Err(ModelError::InputBudgetExceeded(error)) if error.report.as_ref().is_some_and(|report| {
                report.kind == ModelBudgetLimit::GenerationRequestBytes
                    && report.observed_is_lower_bound && report.observed == report.limit + 1
            })
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
