//! Streamable HTTP uses the SDK wire implementation. Endpoint, credentials and
//! replay policy remain host-owned; transport failures never expose peer text.
use crate::{HttpServerConfig, host::McpService, legacy_wire::Negotiation, oauth::OAuthGrant};
use reqwest::{
    Method, StatusCode, Url,
    header::{AUTHORIZATION, HeaderMap, HeaderName, HeaderValue},
};
use rmcp::{
    ClientLifecycleMode, ClientServiceExt,
    model::{ErrorCode, ProtocolVersion},
    service::ClientInitializeError,
    transport::{
        StreamableHttpClientTransport, common::client_side_sse::NeverRetry,
        streamable_http_client::StreamableHttpClientTransportConfig,
    },
};
use std::{
    collections::BTreeSet,
    sync::{
        Arc,
        atomic::{AtomicU16, Ordering},
    },
    time::Duration,
};

mod legacy_sse;
mod legacy_tasks;
pub(crate) mod task_watch;

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
    if matches!(config.transport, crate::HttpTransport::HttpSse)
        && matches!(config.lifecycle, crate::Lifecycle::Discover)
    {
        return Err("legacy MCP HTTP+SSE requires the initialize lifecycle".into());
    }
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
    if let Some(egress) = &config.egress {
        egress.validate()?;
        egress.require(&url)?;
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
    grant: Option<Arc<OAuthGrant>>,
) -> Result<
    (
        McpService,
        Option<Negotiation>,
        Option<task_watch::Endpoint>,
    ),
    String,
> {
    let timeout = Duration::from_millis(config.timeout_ms);
    let streamable = matches!(config.transport, crate::HttpTransport::StreamableHttp);
    let auto = streamable && matches!(config.lifecycle, crate::Lifecycle::Auto);
    let status = Arc::new(AtomicU16::new(0));
    let mut retries = reqwest::retry::never();
    if auto {
        // rmcp preserves the RPC error but can erase its HTTP status. Observe
        // the status via reqwest's classifier, which never requests a retry.
        let status = Arc::clone(&status);
        retries = retries.classify_fn(move |exchange| {
            if exchange.method() == Method::POST {
                status.store(
                    exchange.status().map_or(0, |s| s.as_u16()),
                    Ordering::Relaxed,
                );
            }
            exchange.success()
        });
    }
    let builder = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .retry(retries)
        .no_proxy()
        .connect_timeout(timeout.min(Duration::from_secs(10)))
        .read_timeout(timeout)
        .pool_max_idle_per_host(0)
        .default_headers(headers);
    let builder = match &config.egress {
        Some(egress) => egress.apply(builder),
        None => builder,
    };
    let client = builder
        .build()
        .map_err(|_| "cannot build MCP HTTP client")?;
    if !streamable {
        if grant.is_some() {
            return Err("MCP OAuth requires Streamable HTTP".into());
        }
        return legacy_sse::connect(client, url)
            .await
            .map(|service| (service, None, None));
    }
    let client = legacy_tasks::WireClient {
        client,
        grant,
        negotiation: None,
    };
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
    if matches!(config.lifecycle, crate::Lifecycle::Legacy) {
        return connect_initialize(client, options, ProtocolVersion::LATEST_WITH_INITIALIZE)
            .await
            .map(|(service, negotiation)| (service, negotiation, None));
    }
    let lifecycle = match config.lifecycle {
        // SDK Auto can fall back after a discovery timeout. Silence over HTTP
        // is not evidence of a legacy peer; use the host's startup budget.
        crate::Lifecycle::Auto => crate::Lifecycle::Discover.sdk(),
        mode => mode.sdk(),
    };
    let result = crate::client_config()
        .serve_with_lifecycle(
            StreamableHttpClientTransport::with_client(client.clone(), options.clone()),
            lifecycle,
        )
        .await;
    // A discovery POST is the only POST in flight during modern startup.
    let status = StatusCode::from_u16(status.load(Ordering::Relaxed)).ok();
    if auto
        && let Some(status) = status
        && let Some(version) = result
            .as_ref()
            .err()
            .and_then(|error| legacy_discovery_version(status, error))
    {
        return connect_initialize(client, options, version)
            .await
            .map(|(service, negotiation)| (service, negotiation, None));
    }
    result
        .map(|service| {
            (
                service,
                None,
                Some(task_watch::Endpoint {
                    client: client.client,
                    grant: client.grant,
                    url,
                }),
            )
        })
        .map_err(|_| "MCP HTTP initialization failed".into())
}

async fn connect_initialize(
    mut client: legacy_tasks::WireClient,
    options: StreamableHttpClientTransportConfig,
    version: ProtocolVersion,
) -> Result<(McpService, Option<Negotiation>), String> {
    let config = crate::client_config().with_protocol_version(version.clone());
    let error = |_| "MCP HTTP legacy initialization failed".to_owned();
    if version != ProtocolVersion::V_2025_11_25 {
        return config
            .serve_with_lifecycle(
                StreamableHttpClientTransport::with_client(client, options),
                ClientLifecycleMode::Initialize,
            )
            .await
            .map(|service| (service, None))
            .map_err(error);
    }
    let negotiation = Negotiation::default();
    client.negotiation = Some(negotiation.clone());
    let inner = StreamableHttpClientTransport::with_client(client, options);
    config
        .serve_with_lifecycle(
            legacy_tasks::LegacyTransport {
                inner,
                negotiation: negotiation.clone(),
            },
            ClientLifecycleMode::Initialize,
        )
        .await
        .map(|service| (service, Some(negotiation)))
        .map_err(error)
}

fn legacy_discovery_version(
    status: StatusCode,
    error: &ClientInitializeError,
) -> Option<ProtocolVersion> {
    match error {
        // rmcp normalizes an opaque HTTP rejection to Invalid Request. Do
        // not treat arbitrary JSON-RPC server or modern errors as legacy.
        ClientInitializeError::JsonRpcError(error) => {
            // The official server also maps Method Not Found to HTTP 404.
            // A bare 404 (normalized by rmcp to Invalid Request) is not
            // evidence that this endpoint serves legacy Streamable HTTP.
            ((status == StatusCode::NOT_FOUND && error.code == ErrorCode::METHOD_NOT_FOUND)
                || ((status.is_success() || status == StatusCode::BAD_REQUEST)
                    && matches!(
                        error.code,
                        ErrorCode::METHOD_NOT_FOUND
                            | ErrorCode::INVALID_REQUEST
                            | ErrorCode::INVALID_PARAMS
                    )))
            .then_some(ProtocolVersion::LATEST_WITH_INITIALIZE)
        }
        ClientInitializeError::NoCompatibleProtocolVersion {
            server_supported, ..
        } => {
            // A legacy-only SDK server can return a structured 400 naming its
            // initialize revisions. Follow that explicit version advertisement
            // without sending discover at a revision that needs initialize.
            if !(status.is_success() || status == StatusCode::BAD_REQUEST)
                || server_supported.contains(&ProtocolVersion::V_2026_07_28)
            {
                return None;
            }
            server_supported
                .iter()
                .filter(|version| {
                    version.has_initialize()
                        && **version >= ProtocolVersion::V_2025_03_26
                        && ProtocolVersion::KNOWN_VERSIONS.contains(version)
                })
                .max_by_key(|version| version.as_str())
                .cloned()
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests;
