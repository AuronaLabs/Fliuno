use crate::{Error, Result, SendResponse, StreamEvent, Task, wire::valid_id};
use reqwest::{
    Url,
    header::{ACCEPT, AUTHORIZATION, HeaderMap, HeaderValue},
};
use serde_json::{Value, json};
use std::{
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};
use tokio::sync::mpsc;

const STREAM_QUEUE_CAPACITY: usize = 16;
const MAX_STREAM_EVENTS: usize = 4096;

/// Credentials are host-resolved values. Debug is intentionally not derived.
#[derive(Clone)]
pub struct Options {
    pub endpoint: String,
    pub bearer_token: Option<String>,
    pub request_timeout: Duration,
    pub max_response_bytes: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sse_decoder_handles_bom_comments_crlf_and_chunked_multiline_data() {
        let mut decoder = SseDecoder::default();
        decoder
            .push(b"\xef\xbb\xbf: keepalive\r\n\r\ndata: {\"label\":\r", 1024)
            .unwrap();
        assert_eq!(decoder.next_event(false).unwrap(), None);
        decoder.push(b"\ndata: \"Fliuno\"}\r\n\r\n", 1024).unwrap();
        let event = decoder.next_event(false).unwrap().unwrap();
        assert_eq!(
            serde_json::from_slice::<Value>(&event).unwrap(),
            json!({"label":"Fliuno"})
        );
        assert_eq!(decoder.next_event(false).unwrap(), None);
    }

    #[test]
    fn sse_decoder_dispatches_a_complete_data_line_at_eof() {
        let mut decoder = SseDecoder::default();
        decoder.push(b"data: {\"ok\":true}", 64).unwrap();
        assert_eq!(
            serde_json::from_slice::<Value>(&decoder.next_event(true).unwrap().unwrap()).unwrap(),
            json!({"ok":true})
        );
    }

    #[test]
    fn sse_decoder_rejects_invalid_utf8_and_unbounded_lines() {
        let mut invalid = SseDecoder::default();
        invalid.push(b"data: \xff\n\n", 64).unwrap();
        assert!(matches!(invalid.next_event(false), Err(Error::Protocol)));

        let mut oversized = SseDecoder::default();
        assert!(matches!(
            oversized.push(&[b'x'; 65], 64),
            Err(Error::Budget)
        ));
    }
}
pub fn validate_endpoint(value: &str) -> Result<Url> {
    let url = Url::parse(value).map_err(|_| Error::Config)?;
    let loopback = url.host_str().is_some_and(|host| {
        host.trim_matches(['[', ']'])
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
    });
    if value.len() > 4096
        || value.chars().any(char::is_control)
        || url.host_str().is_none()
        || !(url.scheme() == "https" || url.scheme() == "http" && loopback)
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
        || url.query().is_some()
    {
        return Err(Error::Config);
    }
    Ok(url)
}
pub struct Client {
    http: reqwest::Client,
    stream_http: reqwest::Client,
    endpoint: Url,
    limit: usize,
    next_id: AtomicU64,
}
impl Client {
    pub fn new(options: Options) -> Result<Self> {
        let endpoint = validate_endpoint(&options.endpoint)?;
        if !(Duration::from_millis(100)..=Duration::from_secs(3600))
            .contains(&options.request_timeout)
            || !(1024..=4 * 1024 * 1024).contains(&options.max_response_bytes)
        {
            return Err(Error::Config);
        }
        let mut headers = HeaderMap::new();
        headers.insert(ACCEPT, HeaderValue::from_static("application/json"));
        headers.insert("a2a-version", HeaderValue::from_static("1.0"));
        if let Some(token) = options.bearer_token {
            if token.is_empty() || token.len() > 8192 || token.chars().any(char::is_whitespace) {
                return Err(Error::Config);
            }
            let mut value =
                HeaderValue::from_str(&format!("Bearer {token}")).map_err(|_| Error::Config)?;
            value.set_sensitive(true);
            headers.insert(AUTHORIZATION, value);
        }
        let http = reqwest::Client::builder()
            .default_headers(headers.clone())
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .no_proxy()
            .timeout(options.request_timeout)
            .build()
            .map_err(|_| Error::Config)?;
        let mut stream_headers = headers;
        stream_headers.insert(ACCEPT, HeaderValue::from_static("text/event-stream"));
        let stream_http = reqwest::Client::builder()
            .default_headers(stream_headers)
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .no_proxy()
            .connect_timeout(options.request_timeout)
            .timeout(Duration::from_secs(3600))
            .build()
            .map_err(|_| Error::Config)?;
        Ok(Self {
            http,
            stream_http,
            endpoint,
            limit: options.max_response_bytes,
            next_id: AtomicU64::new(1),
        })
    }
    async fn body(&self, mut response: reqwest::Response) -> Result<Value> {
        if !response.status().is_success() {
            return Err(Error::Http(response.status().as_u16()));
        }
        if response
            .content_length()
            .is_some_and(|n| n > self.limit as u64)
        {
            return Err(Error::Budget);
        }
        if response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.split(';').next())
            .is_none_or(|v| !v.trim().eq_ignore_ascii_case("application/json"))
        {
            return Err(Error::Protocol);
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|_| Error::Transport)? {
            if bytes.len().saturating_add(chunk.len()) > self.limit {
                return Err(Error::Budget);
            }
            bytes.extend_from_slice(&chunk);
        }
        serde_json::from_slice(&bytes).map_err(|_| Error::Protocol)
    }
    async fn rpc(&self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed).to_string();
        let bytes =
            serde_json::to_vec(&json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}))
                .map_err(|_| Error::Protocol)?;
        if bytes.len() > 1024 * 1024 {
            return Err(Error::Budget);
        }
        let response = self
            .http
            .post(self.endpoint.clone())
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(bytes)
            .send()
            .await
            .map_err(|_| Error::Transport)?;
        let raw = self.body(response).await?;
        if raw.get("jsonrpc").and_then(Value::as_str) != Some("2.0")
            || raw.get("id").and_then(Value::as_str) != Some(&id)
            || raw.get("result").is_some() == raw.get("error").is_some()
        {
            return Err(Error::Protocol);
        }
        if let Some(error) = raw.get("error") {
            return Err(Error::Remote(
                error
                    .get("code")
                    .and_then(Value::as_i64)
                    .ok_or(Error::Protocol)?,
            ));
        }
        raw.get("result").cloned().ok_or(Error::Protocol)
    }
    pub async fn send_text(
        &self,
        message_id: &str,
        text: &str,
        history: Option<&str>,
    ) -> Result<SendResponse> {
        if !valid_id(message_id) {
            return Err(Error::Config);
        }
        let mut parts = Vec::new();
        if let Some(history) = history {
            parts.push(json!({"text":history}));
        }
        parts.push(json!({"text":text}));
        SendResponse::parse(
            self.rpc(
                "SendMessage",
                json!({
                    "message":{"messageId":message_id,"role":"ROLE_USER","parts":parts},
                    "configuration":{"returnImmediately":true,"historyLength":0}
                }),
            )
            .await?,
        )
    }

    /// Starts one A2A streaming submission and returns a bounded event receiver.
    /// The caller owns the overall task deadline and cancellation policy.
    pub async fn send_text_streaming(
        &self,
        message_id: &str,
        text: &str,
        history: Option<&str>,
    ) -> Result<mpsc::Receiver<Result<StreamEvent>>> {
        if !valid_id(message_id) {
            return Err(Error::Config);
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed).to_string();
        let mut parts = Vec::new();
        if let Some(history) = history {
            parts.push(json!({"text":history}));
        }
        parts.push(json!({"text":text}));
        let bytes = serde_json::to_vec(&json!({
            "jsonrpc":"2.0",
            "id":id,
            "method":"SendStreamingMessage",
            "params":{
                "message":{"messageId":message_id,"role":"ROLE_USER","parts":parts},
                "configuration":{"returnImmediately":true,"historyLength":0}
            }
        }))
        .map_err(|_| Error::Protocol)?;
        if bytes.len() > 1024 * 1024 {
            return Err(Error::Budget);
        }
        let mut response = self
            .stream_http
            .post(self.endpoint.clone())
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(bytes)
            .send()
            .await
            .map_err(|_| Error::Transport)?;
        if !response.status().is_success() {
            return Err(Error::Http(response.status().as_u16()));
        }
        if response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.split(';').next())
            .is_none_or(|value| !value.trim().eq_ignore_ascii_case("text/event-stream"))
        {
            return Err(Error::Protocol);
        }
        if response
            .content_length()
            .is_some_and(|length| length > self.limit as u64)
        {
            return Err(Error::Budget);
        }
        let (sender, receiver) = mpsc::channel(STREAM_QUEUE_CAPACITY);
        let id = id.clone();
        let limit = self.limit;
        tokio::spawn(async move {
            if let Err(error) = consume_stream(&mut response, &id, limit, sender.clone()).await {
                let _ = sender.send(Err(error)).await;
            }
        });
        Ok(receiver)
    }
    pub async fn get_task(&self, id: &str) -> Result<Task> {
        if !valid_id(id) {
            return Err(Error::Config);
        }
        Task::parse(
            self.rpc("GetTask", json!({"id":id,"historyLength":0}))
                .await?,
        )
    }
    pub async fn cancel_task(&self, id: &str) -> Result<Task> {
        if !valid_id(id) {
            return Err(Error::Config);
        }
        Task::parse(self.rpc("CancelTask", json!({"id":id})).await?)
    }
    /// Optional host-pinned card check. A card cannot change URL, tenant, auth,
    /// version or protocol. Required extensions are unsupported and fail closed.
    pub async fn check_card(&self, card_url: &str) -> Result<()> {
        self.check_card_mode(card_url, false).await
    }

    pub async fn check_card_streaming(&self, card_url: &str) -> Result<()> {
        self.check_card_mode(card_url, true).await
    }

    async fn check_card_mode(&self, card_url: &str, require_streaming: bool) -> Result<()> {
        let url = validate_endpoint(card_url)?;
        if url.origin() != self.endpoint.origin() {
            return Err(Error::Config);
        }
        let card = self
            .body(
                self.http
                    .get(url)
                    .send()
                    .await
                    .map_err(|_| Error::Transport)?,
            )
            .await?;
        let interfaces = card
            .get("supportedInterfaces")
            .and_then(Value::as_array)
            .ok_or(Error::Protocol)?;
        if !interfaces.iter().any(|i| {
            i.get("protocolVersion").and_then(Value::as_str) == Some("1.0")
                && i.get("protocolBinding").and_then(Value::as_str) == Some("JSONRPC")
                && i.get("url")
                    .and_then(Value::as_str)
                    .and_then(|s| validate_endpoint(s).ok())
                    .as_ref()
                    == Some(&self.endpoint)
                && i.get("tenant").is_none_or(|t| t.as_str() == Some(""))
        }) {
            return Err(Error::Protocol);
        }
        if let Some(extensions) = card.pointer("/capabilities/extensions") {
            for extension in extensions.as_array().ok_or(Error::Protocol)? {
                if extension.get("required").is_some_and(|v| v != false) {
                    return Err(Error::Protocol);
                }
            }
        }
        if require_streaming
            && card
                .pointer("/capabilities/streaming")
                .and_then(Value::as_bool)
                != Some(true)
        {
            return Err(Error::Protocol);
        }
        Ok(())
    }
}

async fn consume_stream(
    response: &mut reqwest::Response,
    request_id: &str,
    limit: usize,
    sender: mpsc::Sender<Result<StreamEvent>>,
) -> Result<()> {
    let mut decoder = SseDecoder::default();
    let mut sequence = StreamSequence::default();
    let mut total = 0usize;
    let mut event_count = 0usize;
    while let Some(chunk) = response.chunk().await.map_err(|_| Error::Transport)? {
        total = total.checked_add(chunk.len()).ok_or(Error::Budget)?;
        if total > limit {
            return Err(Error::Budget);
        }
        decoder.push(&chunk, limit)?;
        while let Some(data) = decoder.next_event(false)? {
            event_count += 1;
            if event_count > MAX_STREAM_EVENTS {
                return Err(Error::Budget);
            }
            let event = parse_stream_response(&data, request_id, &mut sequence)?;
            if sender.send(Ok(event)).await.is_err() {
                return Ok(());
            }
        }
    }
    while let Some(data) = decoder.next_event(true)? {
        event_count += 1;
        if event_count > MAX_STREAM_EVENTS {
            return Err(Error::Budget);
        }
        let event = parse_stream_response(&data, request_id, &mut sequence)?;
        if sender.send(Ok(event)).await.is_err() {
            return Ok(());
        }
    }
    if !sequence.is_complete() {
        return Err(Error::Protocol);
    }
    Ok(())
}

#[derive(Default)]
struct SseDecoder {
    pending: Vec<u8>,
    cursor: usize,
    data: Vec<u8>,
    has_data: bool,
    first_line: bool,
}

impl SseDecoder {
    fn push(&mut self, bytes: &[u8], limit: usize) -> Result<()> {
        if self.cursor > 0 {
            self.pending.drain(..self.cursor);
            self.cursor = 0;
        }
        if self.pending.len().saturating_add(bytes.len()) > limit {
            return Err(Error::Budget);
        }
        self.pending.extend_from_slice(bytes);
        Ok(())
    }

    fn next_event(&mut self, eof: bool) -> Result<Option<Vec<u8>>> {
        loop {
            let remaining = &self.pending[self.cursor..];
            let delimiter = remaining
                .iter()
                .position(|byte| matches!(byte, b'\n' | b'\r'));
            let Some(offset) = delimiter else {
                if eof && remaining.is_empty() && self.has_data {
                    self.has_data = false;
                    self.data.pop();
                    return Ok(Some(std::mem::take(&mut self.data)));
                }
                if !eof || remaining.is_empty() {
                    if self.cursor > 0 {
                        self.pending.drain(..self.cursor);
                        self.cursor = 0;
                    }
                    return Ok(None);
                };
                let line = remaining.to_vec();
                self.cursor = self.pending.len();
                if let Some(event) = self.process_line(&line)? {
                    return Ok(Some(event));
                }
                continue;
            };
            let end = self.cursor + offset;
            if self.pending[end] == b'\r' && end + 1 == self.pending.len() && !eof {
                if self.cursor > 0 {
                    self.pending.drain(..self.cursor);
                    self.cursor = 0;
                }
                return Ok(None);
            }
            let line = self.pending[self.cursor..end].to_vec();
            self.cursor = end + 1;
            if self.pending[end] == b'\r'
                && self
                    .pending
                    .get(self.cursor)
                    .is_some_and(|byte| *byte == b'\n')
            {
                self.cursor += 1;
            }
            if let Some(event) = self.process_line(&line)? {
                return Ok(Some(event));
            }
        }
    }

    fn process_line(&mut self, mut line: &[u8]) -> Result<Option<Vec<u8>>> {
        if !self.first_line {
            self.first_line = true;
            if let Some(without_bom) = line.strip_prefix(&[0xef, 0xbb, 0xbf]) {
                line = without_bom;
            }
        }
        let line = std::str::from_utf8(line).map_err(|_| Error::Protocol)?;
        if line.is_empty() {
            if !self.has_data {
                return Ok(None);
            }
            self.data.pop();
            self.has_data = false;
            return Ok(Some(std::mem::take(&mut self.data)));
        }
        if line.starts_with(':') {
            return Ok(None);
        }
        let (field, value) = line.split_once(':').unwrap_or((line, ""));
        let value = value.strip_prefix(' ').unwrap_or(value);
        if field == "data" {
            self.has_data = true;
            self.data.extend_from_slice(value.as_bytes());
            self.data.push(b'\n');
        }
        Ok(None)
    }
}

#[derive(Default)]
struct StreamSequence {
    identity: Option<StreamIdentity>,
}

enum StreamIdentity {
    Task(String, Option<String>, crate::TaskState),
    Message,
}

impl StreamSequence {
    fn accept(&mut self, event: &StreamEvent) -> Result<()> {
        if self.is_complete() {
            return Err(Error::Protocol);
        }
        match (&self.identity, event) {
            (None, StreamEvent::Task(task)) => {
                self.identity = Some(StreamIdentity::Task(
                    task.id.clone(),
                    task.context_id.clone(),
                    task.state,
                ));
            }
            (None, StreamEvent::Message(_)) => {
                self.identity = Some(StreamIdentity::Message);
            }
            (None, _) => return Err(Error::Protocol),
            (
                Some(StreamIdentity::Task(id, context, previous_state)),
                StreamEvent::StatusUpdate {
                    task_id,
                    context_id,
                    state,
                    ..
                },
            ) => {
                if id != task_id
                    || context != context_id
                    || !previous_state.allows_transition_to(*state)
                {
                    return Err(Error::Protocol);
                }
                self.identity = Some(StreamIdentity::Task(id.clone(), context.clone(), *state));
            }
            (
                Some(StreamIdentity::Task(id, context, _)),
                StreamEvent::ArtifactUpdate {
                    task_id,
                    context_id,
                    ..
                },
            ) => {
                if id != task_id || context != context_id {
                    return Err(Error::Protocol);
                }
            }
            _ => return Err(Error::Protocol),
        }
        Ok(())
    }

    fn is_complete(&self) -> bool {
        match self.identity {
            Some(StreamIdentity::Message) => true,
            Some(StreamIdentity::Task(_, _, state)) => state.is_settled(),
            None => false,
        }
    }
}

fn parse_stream_response(
    data: &[u8],
    request_id: &str,
    sequence: &mut StreamSequence,
) -> Result<StreamEvent> {
    let raw: Value = serde_json::from_slice(data).map_err(|_| Error::Protocol)?;
    if raw.get("jsonrpc").and_then(Value::as_str) != Some("2.0")
        || raw.get("id").and_then(Value::as_str) != Some(request_id)
        || raw.get("result").is_some() == raw.get("error").is_some()
    {
        return Err(Error::Protocol);
    }
    if let Some(error) = raw.get("error") {
        return Err(Error::Remote(
            error
                .get("code")
                .and_then(Value::as_i64)
                .ok_or(Error::Protocol)?,
        ));
    }
    let event = StreamEvent::parse(raw.get("result").cloned().ok_or(Error::Protocol)?)?;
    sequence.accept(&event)?;
    Ok(event)
}
