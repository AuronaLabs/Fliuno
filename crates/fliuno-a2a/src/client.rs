use crate::{
    ClientCredentials, Error, Result, SendResponse, StreamEvent, Task, TaskState, oauth::OAuth,
    wire::valid_id,
};
use reqwest::{
    Url,
    header::{ACCEPT, AUTHORIZATION, HeaderMap, HeaderValue},
};
use serde_json::{Value, json};
use std::{
    collections::BTreeSet,
    sync::{
        RwLock,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::sync::mpsc;

const STREAM_QUEUE_CAPACITY: usize = 16;
const MAX_STREAM_EVENTS: usize = 4096;

/// Credentials are host-resolved values. Debug is intentionally not derived.
#[derive(Clone)]
pub struct Options {
    pub endpoint: String,
    pub tenant: Option<String>,
    /// Optional extension URIs to request after the pinned card advertises them.
    pub optional_extensions: Vec<String>,
    pub bearer_token: Option<String>,
    pub oauth: Option<ClientCredentials>,
    pub request_timeout: Duration,
    pub max_response_bytes: usize,
}

#[derive(Clone, Copy)]
pub struct MessageTarget<'a> {
    pub context_id: &'a str,
    pub task_id: Option<&'a str>,
}

fn text_params(
    message_id: &str,
    text: &str,
    history: Option<&str>,
    target: Option<MessageTarget<'_>>,
) -> Result<Value> {
    if !valid_id(message_id)
        || target
            .is_some_and(|t| !valid_id(t.context_id) || t.task_id.is_some_and(|id| !valid_id(id)))
    {
        return Err(Error::Config);
    }
    let mut parts = Vec::new();
    if let Some(history) = history {
        parts.push(json!({"text":history}));
    }
    parts.push(json!({"text":text}));
    let mut message = json!({"messageId":message_id,"role":"ROLE_USER","parts":parts});
    if let Some(target) = target {
        message["contextId"] = json!(target.context_id);
        if let Some(task_id) = target.task_id {
            message["taskId"] = json!(task_id);
        }
    }
    Ok(json!({"message":message,"configuration":{"returnImmediately":true,"historyLength":0}}))
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
    tenant: Option<String>,
    optional_extensions: BTreeSet<String>,
    negotiated_extensions: RwLock<Option<HeaderValue>>,
    oauth: Option<OAuth>,
    limit: usize,
    next_id: AtomicU64,
}
impl Client {
    pub fn new(options: Options) -> Result<Self> {
        let endpoint = validate_endpoint(&options.endpoint)?;
        let extensions = options
            .optional_extensions
            .iter()
            .cloned()
            .collect::<BTreeSet<_>>();
        if !(Duration::from_millis(100)..=Duration::from_secs(3600))
            .contains(&options.request_timeout)
            || !(1024..=4 * 1024 * 1024).contains(&options.max_response_bytes)
            || options.tenant.as_ref().is_some_and(|tenant| {
                tenant.trim().is_empty()
                    || tenant.len() > 256
                    || tenant.chars().any(char::is_control)
            })
            || (options.bearer_token.is_some() && options.oauth.is_some())
            || extensions.len() != options.optional_extensions.len()
            || extensions.len() > 8
            || extensions.iter().any(|uri| !valid_extension_uri(uri))
        {
            return Err(Error::Config);
        }
        let mut headers = HeaderMap::new();
        headers.insert(ACCEPT, HeaderValue::from_static("application/json"));
        headers.insert("a2a-version", HeaderValue::from_static("1.0"));
        if let Some(token) = options.bearer_token {
            headers.insert(AUTHORIZATION, bearer_header(&token)?);
        }
        let oauth = options
            .oauth
            .map(|config| OAuth::new(config, options.request_timeout))
            .transpose()?;
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
            tenant: options.tenant,
            optional_extensions: extensions,
            negotiated_extensions: RwLock::new(None),
            oauth,
            limit: options.max_response_bytes,
            next_id: AtomicU64::new(1),
        })
    }
    async fn body(&self, response: reqwest::Response) -> Result<Value> {
        read_json(response, self.limit).await
    }
    pub async fn authenticate(&self) -> Result<()> {
        if let Some(oauth) = &self.oauth {
            oauth.bearer().await?;
        }
        Ok(())
    }
    async fn authorize(&self, request: reqwest::RequestBuilder) -> Result<reqwest::RequestBuilder> {
        match &self.oauth {
            Some(oauth) => Ok(request.header(AUTHORIZATION, oauth.bearer().await?)),
            None => Ok(request),
        }
    }
    async fn authorize_rpc(
        &self,
        mut request: reqwest::RequestBuilder,
    ) -> Result<reqwest::RequestBuilder> {
        if !self.optional_extensions.is_empty() {
            let header = self
                .negotiated_extensions
                .read()
                .map_err(|_| Error::Config)?
                .clone()
                .ok_or(Error::Config)?;
            if !header.is_empty() {
                request = request.header("a2a-extensions", header);
            }
        }
        self.authorize(request).await
    }
    fn encode_rpc(&self, method: &str, mut params: Value) -> Result<(String, Vec<u8>)> {
        if let Some(tenant) = &self.tenant {
            params["tenant"] = json!(tenant);
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed).to_string();
        let bytes =
            serde_json::to_vec(&json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}))
                .map_err(|_| Error::Protocol)?;
        if bytes.len() > 1024 * 1024 {
            return Err(Error::Budget);
        }
        Ok((id, bytes))
    }
    async fn rpc(&self, method: &str, params: Value) -> Result<Value> {
        let (id, bytes) = self.encode_rpc(method, params)?;
        self.post_json(id, bytes).await
    }
    async fn post_json(&self, id: String, bytes: Vec<u8>) -> Result<Value> {
        let response = self
            .authorize_rpc(self.http.post(self.endpoint.clone()))
            .await?
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(bytes)
            .send()
            .await
            .map_err(|_| Error::Transport)?;
        decode_rpc_result(self.body(response).await?, &id)
    }
    /// Validate and encode before the caller commits to a one-shot send.
    pub fn prepare_text(
        &self,
        message_id: &str,
        text: &str,
        history: Option<&str>,
        target: Option<MessageTarget<'_>>,
        streaming: bool,
    ) -> Result<PreparedMessage<'_>> {
        let method = if streaming {
            "SendStreamingMessage"
        } else {
            "SendMessage"
        };
        let (id, bytes) =
            self.encode_rpc(method, text_params(message_id, text, history, target)?)?;
        Ok(PreparedMessage {
            client: self,
            id,
            bytes,
            streaming,
            existing_task: if streaming {
                target.and_then(|target| {
                    Some(TaskIdentity {
                        id: target.task_id?.to_owned(),
                        context_id: target.context_id.to_owned(),
                    })
                })
            } else {
                None
            },
        })
    }

    pub async fn send_text(
        &self,
        message_id: &str,
        text: &str,
        history: Option<&str>,
    ) -> Result<SendResponse> {
        self.prepare_text(message_id, text, history, None, false)?
            .send()
            .await
    }

    /// Starts a one-shot submission; caller owns the task deadline.
    pub async fn send_text_streaming(
        &self,
        message_id: &str,
        text: &str,
        history: Option<&str>,
    ) -> Result<mpsc::Receiver<Result<StreamEvent>>> {
        self.prepare_text(message_id, text, history, None, true)?
            .send_streaming()
            .await
    }

    /// Observe an existing task. The caller supplies the task's previously
    /// observed identity and owns the wait deadline and receiver.
    pub async fn subscribe_to_task(
        &self,
        task_id: &str,
        context_id: &str,
    ) -> Result<mpsc::Receiver<Result<StreamEvent>>> {
        if !valid_id(task_id) || !valid_id(context_id) {
            return Err(Error::Config);
        }
        let (id, bytes) = self.encode_rpc("SubscribeToTask", json!({"id":task_id}))?;
        self.post_stream(
            id,
            bytes,
            StreamStart::Subscription(TaskIdentity {
                id: task_id.to_owned(),
                context_id: context_id.to_owned(),
            }),
        )
        .await
    }

    async fn post_stream(
        &self,
        id: String,
        bytes: Vec<u8>,
        start: StreamStart,
    ) -> Result<mpsc::Receiver<Result<StreamEvent>>> {
        let mut response = self
            .authorize_rpc(self.stream_http.post(self.endpoint.clone()))
            .await?
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(bytes)
            .send()
            .await
            .map_err(|_| Error::Transport)?;
        if !response.status().is_success() {
            return Err(Error::Http(response.status().as_u16()));
        }
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.split(';').next())
            .map(str::trim)
            .ok_or(Error::Protocol)?;
        if content_type.eq_ignore_ascii_case("application/json") {
            // A peer can reject the subscription before opening an SSE stream.
            decode_rpc_result(self.body(response).await?, &id)?;
            return Err(Error::Protocol);
        }
        if !content_type.eq_ignore_ascii_case("text/event-stream") {
            return Err(Error::Protocol);
        }
        if response
            .content_length()
            .is_some_and(|length| length > self.limit as u64)
        {
            return Err(Error::Budget);
        }
        let (sender, receiver) = mpsc::channel(STREAM_QUEUE_CAPACITY);
        let limit = self.limit;
        tokio::spawn(async move {
            let sequence = StreamSequence {
                start,
                identity: None,
            };
            if let Err(error) =
                consume_stream(&mut response, &id, limit, sequence, sender.clone()).await
            {
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
        *self
            .negotiated_extensions
            .write()
            .map_err(|_| Error::Config)? = None;
        let url = validate_endpoint(card_url)?;
        if url.origin() != self.endpoint.origin() {
            return Err(Error::Config);
        }
        let card = self
            .body(
                self.authorize(self.http.get(url))
                    .await?
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
                && match i.get("tenant") {
                    None => self.tenant.is_none(),
                    Some(tenant) => {
                        tenant.as_str() == Some(self.tenant.as_deref().unwrap_or_default())
                    }
                }
        }) {
            return Err(Error::Protocol);
        }
        let mut negotiated = BTreeSet::new();
        if let Some(extensions) = card.pointer("/capabilities/extensions") {
            for extension in extensions.as_array().ok_or(Error::Protocol)? {
                match extension.get("required").map(Value::as_bool) {
                    Some(Some(true)) => return Err(Error::UnsupportedExtension),
                    None | Some(Some(false)) => {}
                    Some(None) => return Err(Error::Protocol),
                }
                if let Some(uri) = extension.get("uri").and_then(Value::as_str)
                    && self.optional_extensions.contains(uri)
                {
                    negotiated.insert(uri);
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
        let extensions = negotiated.into_iter().collect::<Vec<_>>().join(",");
        *self
            .negotiated_extensions
            .write()
            .map_err(|_| Error::Config)? =
            Some(HeaderValue::from_str(&extensions).map_err(|_| Error::Config)?);
        Ok(())
    }
}

fn valid_extension_uri(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 512
        && value.bytes().all(|b| b.is_ascii_graphic() && b != b',')
        && Url::parse(value).is_ok_and(|uri| uri.username().is_empty() && uri.password().is_none())
}

pub(crate) fn bearer_header(token: &str) -> Result<HeaderValue> {
    if token.is_empty() || token.len() > 8192 || token.chars().any(char::is_whitespace) {
        return Err(Error::Config);
    }
    let mut value = HeaderValue::from_str(&format!("Bearer {token}")).map_err(|_| Error::Config)?;
    value.set_sensitive(true);
    Ok(value)
}

pub(crate) async fn read_json<T: serde::de::DeserializeOwned>(
    mut response: reqwest::Response,
    limit: usize,
) -> Result<T> {
    if !response.status().is_success() {
        return Err(Error::Http(response.status().as_u16()));
    }
    if response.content_length().is_some_and(|n| n > limit as u64) {
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
        if bytes.len().saturating_add(chunk.len()) > limit {
            return Err(Error::Budget);
        }
        bytes.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&bytes).map_err(|_| Error::Protocol)
}

/// Encoded request bound to its originating client and consumed by submission.
pub struct PreparedMessage<'a> {
    client: &'a Client,
    id: String,
    bytes: Vec<u8>,
    streaming: bool,
    existing_task: Option<TaskIdentity>,
}

impl PreparedMessage<'_> {
    pub async fn send(self) -> Result<SendResponse> {
        if self.streaming {
            return Err(Error::Config);
        }
        SendResponse::parse(self.client.post_json(self.id, self.bytes).await?)
    }

    pub async fn send_streaming(self) -> Result<mpsc::Receiver<Result<StreamEvent>>> {
        if !self.streaming {
            return Err(Error::Config);
        }
        self.client
            .post_stream(
                self.id,
                self.bytes,
                StreamStart::Submission(self.existing_task),
            )
            .await
    }
}

async fn consume_stream(
    response: &mut reqwest::Response,
    request_id: &str,
    limit: usize,
    mut sequence: StreamSequence,
    sender: mpsc::Sender<Result<StreamEvent>>,
) -> Result<()> {
    let mut decoder = SseDecoder::default();
    let mut total = 0usize;
    let mut event_count = 0usize;
    loop {
        let chunk = tokio::select! {
            _ = sender.closed() => return Ok(()),
            chunk = response.chunk() => chunk.map_err(|_| Error::Transport)?,
        };
        let Some(chunk) = chunk else {
            break;
        };
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

struct TaskIdentity {
    id: String,
    context_id: String,
}

impl TaskIdentity {
    fn matches(&self, id: &str, context_id: Option<&str>) -> bool {
        self.id == id && context_id == Some(self.context_id.as_str())
    }
}

// Submission ends at the end of the current turn. A subscription observes the
// task itself, including turns resumed by another client while it stays open.
enum StreamStart {
    Submission(Option<TaskIdentity>),
    Subscription(TaskIdentity),
}

struct StreamSequence {
    start: StreamStart,
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
        match (&mut self.identity, event) {
            (None, StreamEvent::Task(task)) => {
                let expected = match &self.start {
                    StreamStart::Submission(expected) => expected.as_ref(),
                    StreamStart::Subscription(expected) => Some(expected),
                };
                if expected.is_some_and(|id| !id.matches(&task.id, task.context_id.as_deref()))
                    || matches!(self.start, StreamStart::Subscription(_))
                        && task.state == TaskState::Unspecified
                {
                    return Err(Error::Protocol);
                }
                self.identity = Some(StreamIdentity::Task(
                    task.id.clone(),
                    task.context_id.clone(),
                    task.state,
                ));
            }
            (None, StreamEvent::Message(_))
                if matches!(self.start, StreamStart::Submission(None)) =>
            {
                self.identity = Some(StreamIdentity::Message);
            }
            // The official Python SDK may omit the Task snapshot when resuming
            // an existing task. Bind the first status to the submitted IDs.
            (
                None,
                StreamEvent::StatusUpdate {
                    task_id,
                    context_id,
                    state,
                    ..
                },
            ) if matches!(&self.start, StreamStart::Submission(Some(expected))
                if expected.matches(task_id, context_id.as_deref()))
                && *state != TaskState::Unspecified =>
            {
                self.identity = Some(StreamIdentity::Task(
                    task_id.clone(),
                    context_id.clone(),
                    *state,
                ));
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
                let resumed = matches!(self.start, StreamStart::Subscription(_))
                    && matches!(
                        *previous_state,
                        TaskState::InputRequired | TaskState::AuthRequired
                    )
                    && *state != TaskState::Unspecified;
                if id != task_id
                    || context != context_id
                    || !(resumed || previous_state.allows_transition_to(*state))
                {
                    return Err(Error::Protocol);
                }
                *previous_state = *state;
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
            Some(StreamIdentity::Task(_, _, state)) => match self.start {
                StreamStart::Subscription(_) => state.is_terminal(),
                StreamStart::Submission(_) => state.is_settled(),
            },
            None => false,
        }
    }
}

fn parse_stream_response(
    data: &[u8],
    request_id: &str,
    sequence: &mut StreamSequence,
) -> Result<StreamEvent> {
    let raw = serde_json::from_slice(data).map_err(|_| Error::Protocol)?;
    let event = StreamEvent::parse(decode_rpc_result(raw, request_id)?)?;
    sequence.accept(&event)?;
    Ok(event)
}

fn decode_rpc_result(mut raw: Value, request_id: &str) -> Result<Value> {
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
    Ok(raw["result"].take())
}
