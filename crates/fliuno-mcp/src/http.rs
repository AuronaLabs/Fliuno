//! Streamable HTTP uses the SDK wire implementation. Endpoint, credentials and
//! replay policy remain host-owned; transport failures never expose peer text.
use crate::HttpServerConfig;
use reqwest::{
    Url,
    header::{AUTHORIZATION, HeaderMap, HeaderName, HeaderValue},
};
use rmcp::{
    ClientLifecycleMode, ClientServiceExt, RoleClient,
    model::ProtocolVersion,
    service::{ClientInitializeError, RunningService},
    transport::{
        StreamableHttpClientTransport, common::client_side_sse::NeverRetry,
        streamable_http_client::StreamableHttpClientTransportConfig,
    },
};
use std::{collections::BTreeSet, sync::Arc, time::Duration};

fn env_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

fn header_name(name: &str) -> Result<HeaderName, String> {
    let header =
        HeaderName::from_bytes(name.as_bytes()).map_err(|_| "invalid MCP HTTP header name")?;
    let name = header.as_str();
    if name.starts_with("mcp-")
        || name.starts_with("proxy-")
        || matches!(
            name,
            "host"
                | "accept"
                | "content-type"
                | "content-length"
                | "content-encoding"
                | "connection"
                | "transfer-encoding"
                | "te"
                | "trailer"
                | "upgrade"
                | "last-event-id"
        )
    {
        return Err("MCP HTTP header conflicts with transport-owned headers".into());
    }
    Ok(header)
}

pub(crate) fn validate(config: &HttpServerConfig) -> Result<Url, String> {
    let url = Url::parse(&config.url).map_err(|_| "invalid MCP HTTP endpoint")?;
    let loopback = url.host_str().is_some_and(|host| {
        host.trim_matches(['[', ']'])
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
    });
    if config.url.len() > 4096
        || config.url.chars().any(char::is_control)
        || url.host_str().is_none()
        || !(url.scheme() == "https" || url.scheme() == "http" && loopback)
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        return Err(
            "MCP endpoint requires HTTPS or literal loopback HTTP, without userinfo or fragments"
                .into(),
        );
    }
    if config
        .bearer_token_env
        .as_ref()
        .is_some_and(|name| !env_name(name))
        || config.headers_env.len() > 32
    {
        return Err("invalid MCP HTTP credential configuration".into());
    }
    let mut names = BTreeSet::new();
    if config.bearer_token_env.is_some() {
        names.insert(AUTHORIZATION.as_str().to_owned());
    }
    for (name, env) in &config.headers_env {
        if !env_name(env) || !names.insert(header_name(name)?.as_str().to_owned()) {
            return Err("invalid or duplicate MCP HTTP header configuration".into());
        }
    }
    Ok(url)
}

pub(crate) fn headers(
    config: &HttpServerConfig,
    mut resolve: impl FnMut(&str) -> Option<String>,
) -> Result<HeaderMap, String> {
    let mut headers = HeaderMap::new();
    let mut bytes = 0;
    let mut insert = |name: HeaderName, text: String| -> Result<(), String> {
        bytes += name.as_str().len() + text.len();
        if text.is_empty() || text.len() > 8192 || bytes > 16 * 1024 {
            return Err("MCP HTTP credential size exceeds its budget".into());
        }
        let mut value =
            HeaderValue::from_str(&text).map_err(|_| "invalid MCP HTTP credential header")?;
        value.set_sensitive(true);
        headers.insert(name, value);
        Ok(())
    };
    if let Some(env) = &config.bearer_token_env {
        let token =
            resolve(env).ok_or("MCP bearer token environment variable is missing or invalid")?;
        if token.is_empty()
            || !token
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-._~+/=".contains(&b))
        {
            return Err("MCP bearer token must be an unprefixed token".into());
        }
        insert(AUTHORIZATION, format!("Bearer {token}"))?;
    }
    for (name, env) in &config.headers_env {
        let value = resolve(env).ok_or("MCP header environment variable is missing or invalid")?;
        insert(header_name(name)?, value)?;
    }
    Ok(headers)
}

pub(crate) async fn connect_with_headers(
    config: &HttpServerConfig,
    url: Url,
    headers: HeaderMap,
) -> Result<RunningService<RoleClient, ()>, String> {
    let timeout = Duration::from_millis(config.timeout_ms);
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
        .no_proxy()
        .connect_timeout(timeout.min(Duration::from_secs(10)))
        .read_timeout(timeout)
        .pool_max_idle_per_host(0)
        .default_headers(headers)
        .build()
        .map_err(|_| "cannot build MCP HTTP client")?;
    let mut options = StreamableHttpClientTransportConfig::with_uri(url.as_str())
        .max_concurrent_requests(1)
        .control_request_timeout(timeout.min(Duration::from_secs(5)))
        .max_sse_event_size(1024 * 1024)
        .max_json_response_size(1024 * 1024)
        .reinit_on_expired_session(false);
    options.channel_buffer_capacity = 16;
    // Neither reconnect nor session recovery may turn an uncertain effect into
    // an implicit second tools/call. Reconnection requires a new host lifecycle.
    options.retry_config = Arc::new(NeverRetry::default());
    let result = ()
        .serve_with_lifecycle(
            StreamableHttpClientTransport::with_client(client.clone(), options.clone()),
            config.lifecycle.sdk(),
        )
        .await;
    // A server may reject the modern version before it can return Method Not
    // Found for discover. Only an explicit advertisement of a shared legacy
    // version permits a fresh initialize transport. No tool request was sent.
    let shared_legacy = match &result {
        Err(ClientInitializeError::NoCompatibleProtocolVersion {
            server_supported, ..
        }) => server_supported.iter().any(|version| {
            version.has_initialize()
                && *version >= ProtocolVersion::V_2025_03_26
                && ProtocolVersion::KNOWN_VERSIONS.contains(version)
        }),
        _ => false,
    };
    if matches!(config.lifecycle, crate::Lifecycle::Auto) && shared_legacy {
        return ()
            .serve_with_lifecycle(
                StreamableHttpClientTransport::with_client(client, options),
                ClientLifecycleMode::Initialize,
            )
            .await
            .map_err(|_| "MCP HTTP legacy initialization failed".into());
    }
    result.map_err(|_| "MCP HTTP initialization failed".into())
}

#[cfg(test)]
mod tests;
