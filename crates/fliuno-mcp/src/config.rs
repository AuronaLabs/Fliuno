//! Host-owned transport and lifecycle choices. No configuration is accepted
//! from model output, and credential values never belong in this data model.
use crate::{EgressPolicy, ProcessIsolation, TIMEOUT};
use fliuno_sandbox::ResourceLimits;
use rmcp::{ClientLifecycleMode, model::ProtocolVersion};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
    time::Duration,
};

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct McpConfig {
    pub servers: Vec<ServerConfig>,
}

/// Bootstrap policy for the trusted host. This file contains no credentials.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct McpOAuthConfig {
    pub servers: BTreeMap<String, crate::oauth::OAuthEndpointConfig>,
}

impl McpConfig {
    pub(crate) fn validate(&self) -> Result<(), String> {
        if self.servers.len() > 32 {
            return Err("at most 32 MCP servers may be configured".into());
        }
        let mut ids = BTreeSet::new();
        for server in &self.servers {
            server.validate()?;
            if !ids.insert(server.id()) {
                return Err("duplicate MCP server ID".into());
            }
        }
        Ok(())
    }

    /// Preflight every configured MCP server before performing OAuth discovery.
    pub fn oauth_clients(
        &self,
        policy: McpOAuthConfig,
    ) -> Result<BTreeMap<String, crate::oauth::OAuthClient>, String> {
        self.build_oauth_clients(policy, |resource, policy, timeout, egress| match egress {
            Some(egress) => {
                crate::oauth::OAuthClient::new_with_egress(resource, policy, timeout, egress)
            }
            None => crate::oauth::OAuthClient::new(resource, policy, timeout),
        })
    }

    #[cfg(feature = "test-fixtures")]
    pub fn oauth_fixture_clients(
        &self,
        policy: McpOAuthConfig,
    ) -> Result<BTreeMap<String, crate::oauth::OAuthClient>, String> {
        self.build_oauth_clients(policy, crate::oauth::OAuthClient::loopback_fixture)
    }

    fn build_oauth_clients(
        &self,
        policy: McpOAuthConfig,
        build: impl Fn(
            &str,
            &crate::oauth::OAuthEndpointConfig,
            Duration,
            Option<&EgressPolicy>,
        ) -> Result<crate::oauth::OAuthClient, crate::oauth::OAuthError>,
    ) -> Result<BTreeMap<String, crate::oauth::OAuthClient>, String> {
        self.validate()?;
        if policy.servers.is_empty() || policy.servers.len() > self.servers.len() {
            return Err("invalid number of MCP OAuth server policies".into());
        }
        policy
            .servers
            .into_iter()
            .map(|(id, policy)| {
                let server = self
                    .servers
                    .iter()
                    .find(|server| server.id() == id)
                    .ok_or("MCP OAuth policy has no configured server")?;
                let (http, url) = crate::oauth_server(server)?;
                let client = build(
                    url.as_str(),
                    &policy,
                    Duration::from_millis(http.timeout_ms),
                    http.egress.as_ref(),
                )
                .map_err(|error| error.to_string())?;
                Ok((id, client))
            })
            .collect()
    }
}

/// Untagged preserves existing stdio configuration; HTTP requires an explicit
/// transport marker and cannot fall through to a process launch.
#[derive(Deserialize, Serialize)]
#[serde(untagged)]
pub enum ServerConfig {
    Stdio(StdioServerConfig),
    Http(HttpServerConfig),
}

#[derive(Debug, Default, Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StdioTransport {
    #[default]
    Stdio,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HttpTransport {
    StreamableHttp,
    HttpSse,
}

#[derive(Debug, Default, Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Lifecycle {
    #[default]
    Legacy,
    Discover,
    Auto,
}

impl Lifecycle {
    pub(crate) fn sdk(self) -> ClientLifecycleMode {
        match self {
            Self::Legacy => ClientLifecycleMode::Initialize,
            Self::Discover => ClientLifecycleMode::Discover {
                preferred_versions: vec![ProtocolVersion::V_2026_07_28],
            },
            Self::Auto => ClientLifecycleMode::Auto {
                preferred_versions: vec![ProtocolVersion::V_2026_07_28],
                legacy_version: Some(ProtocolVersion::LATEST_WITH_INITIALIZE),
            },
        }
    }
}

fn auto_lifecycle() -> Lifecycle {
    Lifecycle::Auto
}

fn default_timeout_ms() -> u64 {
    TIMEOUT.as_millis() as u64
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct StdioServerConfig {
    #[serde(default)]
    pub assets: Vec<fliuno_sandbox::StagedFile>,
    pub id: String,
    #[serde(default)]
    pub transport: StdioTransport,
    pub command: PathBuf,
    #[serde(default)]
    pub args: Vec<String>,
    pub isolation: ProcessIsolation,
    /// Optional hard OS resource ceilings for this isolated server.
    #[serde(default)]
    pub resources: ResourceLimits,
    /// Optional delegated cgroup v2 parent used by the Linux sandbox.
    #[serde(default)]
    pub linux_cgroup_parent: Option<PathBuf>,
    /// Required for trusted_host; forbidden for private scratch isolation.
    pub cwd: Option<PathBuf>,
    /// Names of additional inherited variables, never literal credentials.
    #[serde(default)]
    pub env_allow: Vec<String>,
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,
    /// Probe modern lifecycle first, then safely fall back to initialize.
    #[serde(default = "auto_lifecycle")]
    pub lifecycle: Lifecycle,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct HttpServerConfig {
    pub id: String,
    pub transport: HttpTransport,
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub egress: Option<EgressPolicy>,
    #[serde(default)]
    pub bearer_token_env: Option<String>,
    /// Header name -> environment variable name, including vendor API headers.
    #[serde(default)]
    pub headers_env: BTreeMap<String, String>,
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,
    #[serde(default = "auto_lifecycle")]
    pub lifecycle: Lifecycle,
}

impl ServerConfig {
    pub(crate) fn id(&self) -> &str {
        match self {
            Self::Stdio(c) => &c.id,
            Self::Http(c) => &c.id,
        }
    }
    pub(crate) fn timeout_ms(&self) -> u64 {
        match self {
            Self::Stdio(c) => c.timeout_ms,
            Self::Http(c) => c.timeout_ms,
        }
    }
    pub(crate) fn validate(&self) -> Result<(), String> {
        let id = self.id();
        if id.is_empty()
            || id.len() > 16
            || !id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
        {
            return Err("MCP ID must be 1..16 ASCII letters, digits or underscores".into());
        }
        if !(100..=300_000).contains(&self.timeout_ms()) {
            return Err("MCP timeoutMs must be between 100 and 300000".into());
        }
        match self {
            Self::Stdio(c) => crate::launch::validate(c),
            Self::Http(c) => crate::http::validate(c).map(|_| ()),
        }
    }
}
