use crate::{Error, Result, SendResponse, Task, wire::valid_id};
use reqwest::{
    Url,
    header::{ACCEPT, AUTHORIZATION, HeaderMap, HeaderValue},
};
use serde_json::{Value, json};
use std::{
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

/// Credentials are host-resolved values. Debug is intentionally not derived.
#[derive(Clone)]
pub struct Options {
    pub endpoint: String,
    pub bearer_token: Option<String>,
    pub request_timeout: Duration,
    pub max_response_bytes: usize,
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
            .default_headers(headers)
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .no_proxy()
            .timeout(options.request_timeout)
            .build()
            .map_err(|_| Error::Config)?;
        Ok(Self {
            http,
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
        Ok(())
    }
}
