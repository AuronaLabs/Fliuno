//! MCP OAuth for public native clients. The host owns the server selection
//! and browser callback; an MCP peer cannot nominate a credential recipient.
mod metadata;
mod vault;
pub use vault::CredentialVault;

use crate::EgressPolicy;
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use metadata::Discovery;
use reqwest::{Client, Response, Url, header::HeaderValue};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fmt,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::sync::Mutex;
use uuid::Uuid;

const MAX_BODY: usize = 64 * 1024;
const MAX_TOKEN: usize = 16 * 1024;
const MAX_LOGIN_AGE: Duration = Duration::from_secs(600);

/// Host configuration. This type never contains credentials or browser state.
#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct OAuthEndpointConfig {
    pub issuer: String,
    pub redirect_uri: String,
    pub client_id: Option<String>,
    pub client_metadata_url: Option<String>,
    #[serde(default)]
    pub allow_dynamic_registration: bool,
    #[serde(default)]
    pub authorization_endpoint_origins: Vec<String>,
    #[serde(default)]
    pub scopes: Vec<String>,
}

/// Deliberately carries no remote body, headers, URL, or underlying I/O error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OAuthError {
    UnexpectedEndpoint,
    MissingMetadata,
    MetadataRejected,
    IssuerMismatch,
    UnsupportedFlow,
    RegistrationRejected,
    InvalidCallback,
    TokenRejected,
    ReauthorizationRequired,
    CredentialStore,
    Transport,
}

impl fmt::Display for OAuthError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::UnexpectedEndpoint => "MCP OAuth endpoint is outside the configured policy",
            Self::MissingMetadata => "MCP OAuth metadata is unavailable",
            Self::MetadataRejected => "MCP OAuth metadata is invalid or was rejected",
            Self::IssuerMismatch => "MCP OAuth resource or issuer does not match the configuration",
            Self::UnsupportedFlow => {
                "MCP OAuth requires the configured public client and S256 authorization code flow"
            }
            Self::RegistrationRejected => "MCP OAuth public client registration was rejected",
            Self::InvalidCallback => "MCP OAuth callback is invalid or expired",
            Self::TokenRejected => "MCP OAuth token response was rejected",
            Self::ReauthorizationRequired => "MCP OAuth authorization is required",
            Self::CredentialStore => "MCP OAuth credential storage is unavailable or changed",
            Self::Transport => "MCP OAuth HTTP request did not complete",
        })
    }
}
impl std::error::Error for OAuthError {}

#[derive(Clone)]
pub struct OAuthClient {
    http: Client,
    resource: Url,
    issuer: Url,
    issuer_id: String,
    redirect: Url,
    client_id: Option<String>,
    client_metadata_url: Option<Url>,
    allow_dynamic_registration: bool,
    endpoint_origins: Vec<reqwest::Url>,
    scopes: Vec<String>,
    egress: Option<EgressPolicy>,
    policy_id: String,
    vault: Option<vault::Slot>,
}

impl OAuthClient {
    pub fn new(
        resource: &str,
        config: &OAuthEndpointConfig,
        timeout: Duration,
    ) -> Result<Self, OAuthError> {
        Self::build(resource, config, timeout, None, false)
    }

    /// Build a client whose resource, issuer and metadata connections only
    /// use the address pins shared by the configured MCP transport.
    pub fn new_with_egress(
        resource: &str,
        config: &OAuthEndpointConfig,
        timeout: Duration,
        egress: &EgressPolicy,
    ) -> Result<Self, OAuthError> {
        Self::build(resource, config, timeout, Some(egress), false)
    }

    #[cfg(feature = "test-fixtures")]
    pub(crate) fn loopback_fixture(
        resource: &str,
        config: &OAuthEndpointConfig,
        timeout: Duration,
        egress: Option<&EgressPolicy>,
    ) -> Result<Self, OAuthError> {
        let client = Self::build(resource, config, timeout, egress, true)?;
        if client.issuer.scheme() != "http"
            || client.resource.origin() != client.issuer.origin()
            || client.redirect.origin() != client.resource.origin()
            || client.client_metadata_url.is_some()
            || !config.authorization_endpoint_origins.is_empty()
        {
            return Err(OAuthError::UnexpectedEndpoint);
        }
        Ok(client)
    }

    fn build(
        resource: &str,
        config: &OAuthEndpointConfig,
        timeout: Duration,
        egress: Option<&EgressPolicy>,
        test_loopback_issuer: bool,
    ) -> Result<Self, OAuthError> {
        metadata::validate_config(config)?;
        let resource = endpoint(resource, true)?;
        let issuer = endpoint(&config.issuer, test_loopback_issuer)?;
        let redirect = endpoint(&config.redirect_uri, true)?;
        if resource.query().is_some() || issuer.query().is_some() || redirect.query().is_some() {
            return Err(OAuthError::UnexpectedEndpoint);
        }
        let client_metadata_url = config
            .client_metadata_url
            .as_deref()
            .map(|url| endpoint(url, false))
            .transpose()?;
        if client_metadata_url
            .as_ref()
            .is_some_and(|url| url.path() == "/" || url.query().is_some())
            || config
                .client_id
                .as_ref()
                .is_some_and(|id| !bounded_ascii(id, 4096))
        {
            return Err(OAuthError::UnsupportedFlow);
        }
        let mut endpoint_origins = vec![issuer.clone()];
        for origin in &config.authorization_endpoint_origins {
            let origin = endpoint(origin, test_loopback_issuer)?;
            if origin.path() != "/" || origin.query().is_some() {
                return Err(OAuthError::UnexpectedEndpoint);
            }
            endpoint_origins.push(origin);
        }
        let mut builder = Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            // A grant can be refreshed on a different host runtime.
            .pool_max_idle_per_host(0)
            .connect_timeout(timeout.min(Duration::from_secs(10)))
            .timeout(timeout);
        let mut identity = json!({"resource": resource.as_str(), "policy": config});
        if let Some(egress) = egress {
            egress
                .validate()
                .map_err(|_| OAuthError::UnexpectedEndpoint)?;
            for url in std::iter::once(&resource)
                .chain(&endpoint_origins)
                .chain(client_metadata_url.as_ref())
            {
                egress
                    .require(url)
                    .map_err(|_| OAuthError::UnexpectedEndpoint)?;
            }
            builder = egress.apply(builder);
            identity["egress"] = json!(egress);
        }
        let http = builder.build().map_err(|_| OAuthError::Transport)?;
        let policy_id = fliuno_core::authority_digest("mcp.oauth.policy.v1", &identity);
        Ok(Self {
            http,
            resource,
            issuer,
            issuer_id: config.issuer.clone(),
            redirect,
            client_id: config.client_id.clone(),
            client_metadata_url,
            allow_dynamic_registration: config.allow_dynamic_registration,
            endpoint_origins,
            scopes: config.scopes.clone(),
            egress: egress.cloned(),
            policy_id,
            vault: None,
        })
    }

    pub fn with_vault(mut self, vault: &CredentialVault, server_id: &str) -> Self {
        let slot = fliuno_core::authority_digest(
            "mcp.oauth.slot.v1",
            &json!({
                "policy": self.policy_id, "server": server_id,
            }),
        );
        self.vault = Some(vault.slot(slot));
        self
    }

    /// Restore the same authorization identity, or request browser approval
    /// again if no usable grant survives.
    pub async fn restore(&self) -> Result<Option<OAuthGrant>, OAuthError> {
        let Some(slot) = &self.vault else {
            return Ok(None);
        };
        let Some(record) = slot.read().await?.grant else {
            return Ok(None);
        };
        let id = Uuid::parse_str(&record.grant_id).map_err(|_| OAuthError::CredentialStore)?;
        let token_endpoint = self.authorization_endpoint(&record.token_endpoint)?;
        if !bounded_ascii(&record.client_id, 4096)
            || self
                .client_id
                .as_ref()
                .is_some_and(|id| *id != record.client_id)
        {
            return Err(OAuthError::CredentialStore);
        }
        let tokens = record.tokens()?;
        if tokens.needs_refresh()? && tokens.refresh.is_none() {
            return Ok(None);
        }
        Ok(Some(OAuthGrant {
            id,
            client: self.clone(),
            client_id: record.client_id,
            token_endpoint,
            tokens: Mutex::new(None),
        }))
    }

    fn authorization_endpoint(&self, value: &str) -> Result<Url, OAuthError> {
        let url = endpoint(value, self.issuer.scheme() == "http")?;
        if self
            .endpoint_origins
            .iter()
            .any(|origin| origin.origin() == url.origin())
        {
            Ok(url)
        } else {
            Err(OAuthError::UnexpectedEndpoint)
        }
    }

    /// Prepare one browser round trip. Pass the actual MCP 401 challenge when
    /// available. The returned attempt is consumed even on callback failure.
    pub async fn begin(
        &self,
        www_authenticate: Option<&str>,
    ) -> Result<AuthorizationAttempt, OAuthError> {
        let vault_revision = match &self.vault {
            Some(slot) => slot.read().await?.revision,
            None => None,
        };
        let discovery = self.discover(www_authenticate).await?;
        let client_id = self.select_client(&discovery).await?;
        let verifier = format!(
            "{}{}{}",
            Uuid::new_v4().simple(),
            Uuid::new_v4().simple(),
            Uuid::new_v4().simple()
        );
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
        let state = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
        let mut url = discovery.authorize;
        const RESERVED: &[&str] = &[
            "response_type",
            "client_id",
            "redirect_uri",
            "resource",
            "scope",
            "state",
            "code_challenge",
            "code_challenge_method",
        ];
        if url
            .query_pairs()
            .any(|(name, _)| RESERVED.contains(&name.as_ref()))
        {
            return Err(OAuthError::MetadataRejected);
        }
        url.query_pairs_mut().extend_pairs([
            ("response_type", "code"),
            ("client_id", &client_id),
            ("redirect_uri", self.redirect.as_str()),
            ("resource", self.resource.as_str()),
            ("state", &state),
            ("code_challenge", &challenge),
            ("code_challenge_method", "S256"),
        ]);
        if !discovery.scopes.is_empty() {
            url.query_pairs_mut()
                .append_pair("scope", &discovery.scopes.join(" "));
        }
        Ok(AuthorizationAttempt {
            client: self.clone(),
            url,
            client_id,
            token_endpoint: discovery.token,
            expected_issuer: discovery.requires_callback_issuer,
            scopes: discovery.scopes,
            started: Instant::now(),
            state,
            verifier,
            vault_revision,
        })
    }

    async fn select_client(&self, discovery: &Discovery) -> Result<String, OAuthError> {
        if let Some(id) = &self.client_id {
            return Ok(id.clone());
        }
        if discovery.supports_cimd {
            let url = self
                .client_metadata_url
                .as_ref()
                .ok_or(OAuthError::UnsupportedFlow)?;
            let document: ClientDocument = self
                .get_json(url)
                .await?
                .ok_or(OAuthError::MissingMetadata)?;
            if document.client_id != url.as_str()
                || document.client_name.trim().is_empty()
                || !document
                    .redirect_uris
                    .iter()
                    .any(|redirect| redirect == self.redirect.as_str())
                || document
                    .token_endpoint_auth_method
                    .as_deref()
                    .is_some_and(|method| method != "none")
            {
                return Err(OAuthError::RegistrationRejected);
            }
            return Ok(url.to_string());
        }
        let url = discovery
            .registration
            .as_ref()
            .ok_or(OAuthError::UnsupportedFlow)?;
        let response = self.http.post(url.clone()).json(&json!({
            "client_name":"Fliuno", "redirect_uris":[self.redirect.as_str()], "application_type":"native",
            "token_endpoint_auth_method":"none", "grant_types":["authorization_code", "refresh_token"],
            "response_types":["code"], "scope":discovery.scopes.join(" ")
        })).send().await.map_err(|_| OAuthError::Transport)?;
        if !response.status().is_success() {
            return Err(OAuthError::RegistrationRejected);
        }
        let registration: Registration = read_json(response)
            .await
            .map_err(|_| OAuthError::RegistrationRejected)?;
        if !bounded_ascii(&registration.client_id, 4096)
            || registration.client_secret.is_some()
            || registration.token_endpoint_auth_method.as_deref() != Some("none")
            || !registration
                .redirect_uris
                .iter()
                .any(|redirect| redirect == self.redirect.as_str())
        {
            return Err(OAuthError::RegistrationRejected);
        }
        Ok(registration.client_id)
    }

    async fn exchange(
        &self,
        url: &Url,
        form: &[(&str, &str)],
        scopes: &[String],
    ) -> Result<Tokens, OAuthError> {
        let response = self
            .http
            .post(url.clone())
            .header(reqwest::header::ACCEPT, "application/json")
            .form(&form)
            .send()
            .await
            .map_err(|_| OAuthError::Transport)?;
        if !response.status().is_success() {
            return Err(OAuthError::TokenRejected);
        }
        let wire: TokenResponse = read_json(response)
            .await
            .map_err(|_| OAuthError::TokenRejected)?;
        Tokens::from_wire(wire, scopes)
    }
}

#[derive(Deserialize)]
struct ClientDocument {
    client_id: String,
    client_name: String,
    redirect_uris: Vec<String>,
    token_endpoint_auth_method: Option<String>,
}

#[derive(Deserialize)]
struct Registration {
    client_id: String,
    client_secret: Option<String>,
    redirect_uris: Vec<String>,
    token_endpoint_auth_method: Option<String>,
}

/// In-memory authorization attempt. Never persist the verifier, code, or state.
pub struct AuthorizationAttempt {
    client: OAuthClient,
    url: Url,
    client_id: String,
    token_endpoint: Url,
    expected_issuer: bool,
    scopes: Vec<String>,
    started: Instant,
    state: String,
    verifier: String,
    vault_revision: Option<String>,
}

impl AuthorizationAttempt {
    pub fn authorization_url(&self) -> &Url {
        &self.url
    }

    pub async fn finish(self, callback: &str) -> Result<OAuthGrant, OAuthError> {
        if self.started.elapsed() > MAX_LOGIN_AGE || callback.len() > 8192 {
            return Err(OAuthError::InvalidCallback);
        }
        let mut received = Url::parse(callback).map_err(|_| OAuthError::InvalidCallback)?;
        let mut fields = BTreeMap::new();
        for (name, value) in received.query_pairs() {
            if fields
                .insert(name.into_owned(), value.into_owned())
                .is_some()
            {
                return Err(OAuthError::InvalidCallback);
            }
        }
        received.set_query(None);
        if received != self.client.redirect
            || fields.contains_key("error")
            || fields.get("state") != Some(&self.state)
            || fields
                .get("iss")
                .map_or(self.expected_issuer, |iss| iss != &self.client.issuer_id)
        {
            return Err(OAuthError::InvalidCallback);
        }
        let code = fields
            .get("code")
            .filter(|code| bounded_ascii(code, 4096))
            .ok_or(OAuthError::InvalidCallback)?;
        let tokens = self
            .client
            .exchange(
                &self.token_endpoint,
                &[
                    ("grant_type", "authorization_code"),
                    ("code", code),
                    ("client_id", &self.client_id),
                    ("redirect_uri", self.client.redirect.as_str()),
                    ("resource", self.client.resource.as_str()),
                    ("code_verifier", &self.verifier),
                ],
                &self.scopes,
            )
            .await?;
        let id = Uuid::new_v4();
        let tokens = if let Some(slot) = &self.client.vault {
            let record = StoredGrant::new(id, &self.client_id, &self.token_endpoint, &tokens)?;
            slot.replace(self.vault_revision, Some(record)).await?;
            None
        } else {
            Some(tokens)
        };
        Ok(OAuthGrant {
            id,
            client: self.client,
            client_id: self.client_id,
            token_endpoint: self.token_endpoint,
            tokens: Mutex::new(tokens),
        })
    }
}

/// One grant, bound to the discovered issuer and exact MCP resource URL.
pub struct OAuthGrant {
    id: Uuid,
    client: OAuthClient,
    client_id: String,
    token_endpoint: Url,
    tokens: Mutex<Option<Tokens>>,
}

impl OAuthGrant {
    pub(crate) fn binding_for_server(
        &self,
        target: &Url,
        egress: Option<&EgressPolicy>,
    ) -> Result<Uuid, OAuthError> {
        if self.client.egress.as_ref() != egress {
            return Err(OAuthError::UnexpectedEndpoint);
        }
        self.binding(target)
    }

    pub(crate) fn binding(&self, target: &Url) -> Result<Uuid, OAuthError> {
        if *target != self.client.resource {
            return Err(OAuthError::UnexpectedEndpoint);
        }
        Ok(self.id)
    }

    /// Refresh before a new MCP operation. This API never resends an MCP call.
    pub async fn authorization(&self, target: &Url) -> Result<HeaderValue, OAuthError> {
        self.binding(target)?;
        let mut saved = self.tokens.lock().await;
        if let Some(slot) = &self.client.vault {
            let snapshot = slot.read().await?;
            let record = snapshot.grant.ok_or(OAuthError::ReauthorizationRequired)?;
            if record.grant_id != self.id.to_string()
                || record.client_id != self.client_id
                || record.token_endpoint != self.token_endpoint.as_str()
            {
                return Err(OAuthError::ReauthorizationRequired);
            }
            let tokens = record.tokens()?;
            if !tokens.needs_refresh()? {
                return Ok(tokens.authorization);
            }
            if tokens.refresh.is_none() {
                return Err(OAuthError::ReauthorizationRequired);
            }
            // Commit consumption before network I/O. A stale writer cannot
            // finalize over a new authorization or another refresh.
            let consumed = slot.replace(snapshot.revision, None).await?;
            let next = self.refresh(tokens).await?;
            let record = StoredGrant::new(self.id, &self.client_id, &self.token_endpoint, &next)?;
            slot.replace(Some(consumed), Some(record)).await?;
            return Ok(next.authorization);
        }
        let current = saved.as_ref().ok_or(OAuthError::ReauthorizationRequired)?;
        if current.needs_refresh()? {
            // Consume before I/O. A rotating refresh token with an unknown
            // outcome must not be sent again, including on caller cancellation.
            *saved = Some(self.refresh(saved.take().expect("checked grant")).await?);
        }
        Ok(saved
            .as_ref()
            .expect("checked or replaced grant")
            .authorization
            .clone())
    }

    async fn refresh(&self, current: Tokens) -> Result<Tokens, OAuthError> {
        let refresh = current.refresh.ok_or(OAuthError::ReauthorizationRequired)?;
        let mut next = self
            .client
            .exchange(
                &self.token_endpoint,
                &[
                    ("grant_type", "refresh_token"),
                    ("refresh_token", &refresh),
                    ("client_id", &self.client_id),
                    ("resource", self.client.resource.as_str()),
                ],
                &current.scopes,
            )
            .await?;
        // A confirmed success may omit a replacement refresh token.
        if next.refresh.is_none() {
            next.refresh = Some(refresh);
        }
        Ok(next)
    }
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    token_type: String,
    refresh_token: Option<String>,
    expires_in: Option<u64>,
    scope: Option<String>,
}

struct Tokens {
    authorization: HeaderValue,
    refresh: Option<String>,
    issued_ms: u64,
    refresh_at_ms: Option<u64>,
    scopes: Vec<String>,
}

impl Tokens {
    fn needs_refresh(&self) -> Result<bool, OAuthError> {
        let now = clock_ms()?;
        Ok(now < self.issued_ms || self.refresh_at_ms.is_some_and(|deadline| now >= deadline))
    }

    fn from_wire(wire: TokenResponse, requested: &[String]) -> Result<Self, OAuthError> {
        let bearer = wire.access_token.trim_end_matches('=');
        if !wire.token_type.eq_ignore_ascii_case("bearer")
            || bearer.is_empty()
            || wire.access_token.len() > MAX_TOKEN
            || !bearer
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-._~+/".contains(&b))
            || wire
                .refresh_token
                .as_ref()
                .is_some_and(|token| !bounded_ascii(token, MAX_TOKEN))
            || wire
                .expires_in
                .is_some_and(|seconds| seconds > 365 * 24 * 60 * 60)
        {
            return Err(OAuthError::TokenRejected);
        }
        let mut authorization = HeaderValue::from_str(&format!("Bearer {}", wire.access_token))
            .map_err(|_| OAuthError::TokenRejected)?;
        authorization.set_sensitive(true);
        let scopes = wire.scope.map_or_else(
            || requested.to_vec(),
            |scope| scope.split_ascii_whitespace().map(str::to_owned).collect(),
        );
        metadata::validate_scopes(&scopes)?;
        if !requested.is_empty() && scopes.iter().any(|scope| !requested.contains(scope)) {
            return Err(OAuthError::TokenRejected);
        }
        let issued_ms = clock_ms()?;
        let refresh_at_ms = wire.expires_in.map(|seconds| {
            let ttl_ms = seconds * 1000;
            issued_ms + ttl_ms - (ttl_ms / 10).min(30_000)
        });
        Ok(Self {
            authorization,
            refresh: wire.refresh_token,
            issued_ms,
            refresh_at_ms,
            scopes,
        })
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredGrant {
    grant_id: String,
    client_id: String,
    token_endpoint: String,
    access: String,
    refresh: Option<String>,
    scopes: Vec<String>,
    issued_ms: u64,
    refresh_at_ms: Option<u64>,
}

impl StoredGrant {
    fn new(id: Uuid, client_id: &str, endpoint: &Url, tokens: &Tokens) -> Result<Self, OAuthError> {
        let access = tokens
            .authorization
            .to_str()
            .ok()
            .and_then(|s| s.strip_prefix("Bearer "))
            .ok_or(OAuthError::CredentialStore)?;
        Ok(Self {
            grant_id: id.to_string(),
            client_id: client_id.into(),
            token_endpoint: endpoint.as_str().into(),
            access: access.into(),
            refresh: tokens.refresh.clone(),
            scopes: tokens.scopes.clone(),
            issued_ms: tokens.issued_ms,
            refresh_at_ms: tokens.refresh_at_ms,
        })
    }

    fn tokens(&self) -> Result<Tokens, OAuthError> {
        let mut tokens = Tokens::from_wire(
            TokenResponse {
                access_token: self.access.clone(),
                refresh_token: self.refresh.clone(),
                token_type: "Bearer".into(),
                expires_in: None,
                scope: None,
            },
            &self.scopes,
        )?;
        tokens.issued_ms = self.issued_ms;
        tokens.refresh_at_ms = self.refresh_at_ms;
        Ok(tokens)
    }
}

fn clock_ms() -> Result<u64, OAuthError> {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| OAuthError::TokenRejected)?
        .as_millis();
    u64::try_from(millis).map_err(|_| OAuthError::TokenRejected)
}

fn bounded_ascii(value: &str, max: usize) -> bool {
    !value.is_empty() && value.len() <= max && value.bytes().all(|byte| byte.is_ascii_graphic())
}

fn endpoint(value: &str, allow_loopback: bool) -> Result<Url, OAuthError> {
    if value.len() > 4096
        || value
            .chars()
            .any(|ch| ch.is_control() || ch.is_whitespace())
    {
        return Err(OAuthError::UnexpectedEndpoint);
    }
    let url = Url::parse(value).map_err(|_| OAuthError::UnexpectedEndpoint)?;
    let loopback = url.host_str().is_some_and(|host| {
        host.trim_matches(['[', ']'])
            .parse::<std::net::IpAddr>()
            .is_ok_and(|address| address.is_loopback())
    });
    if url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
        || !(url.scheme() == "https" || allow_loopback && loopback && url.scheme() == "http")
    {
        return Err(OAuthError::UnexpectedEndpoint);
    }
    Ok(url)
}

async fn read_json<T: DeserializeOwned>(mut response: Response) -> Result<T, OAuthError> {
    if response
        .content_length()
        .is_some_and(|size| size > MAX_BODY as u64)
    {
        return Err(OAuthError::MetadataRejected);
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| OAuthError::Transport)? {
        if chunk.len() > MAX_BODY - body.len() {
            return Err(OAuthError::MetadataRejected);
        }
        body.extend(&chunk);
    }
    serde_json::from_slice(&body).map_err(|_| OAuthError::MetadataRejected)
}

#[cfg(test)]
mod tests;
