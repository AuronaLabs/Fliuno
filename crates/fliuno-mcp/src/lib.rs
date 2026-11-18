//! MCP client adapter using the official Rust SDK. Servers are trusted host
//! configuration, not model-created commands. Host and isolated launch modes
//! are explicit and never fall back to one another.
mod config;
mod content;
mod discovery;
mod host;
mod http;
mod launch;
mod registration;
pub use config::{
    HttpServerConfig, HttpTransport, Lifecycle, McpConfig, ServerConfig, StdioServerConfig,
    StdioTransport,
};
use fliuno_core::{
    CancellationToken, Kernel, Tool, ToolContext, ToolEffect, ToolError, ToolOutput, ToolSpec,
};
use fliuno_protocol::ToolCall;
use host::Host;
pub use launch::ProcessIsolation;
use rmcp::model::{CallToolRequestParams, CallToolResponse};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{collections::BTreeSet, sync::Arc, time::Duration};
use tokio::sync::Mutex;

const TIMEOUT: Duration = Duration::from_secs(30);

pub fn register_config(kernel: &mut Kernel, config: McpConfig) -> Result<(), String> {
    let mut ids = BTreeSet::new();
    if config.servers.len() > 32 {
        return Err("at most 32 MCP servers may be configured".into());
    }
    // Reject invalid configuration before starting any child or network IO.
    for server in &config.servers {
        server.validate()?;
        if !ids.insert(server.id()) {
            return Err("duplicate MCP server ID".into());
        }
    }
    let mut names: BTreeSet<_> = kernel.tool_specs().into_iter().map(|s| s.name).collect();
    let mut staged = Vec::new();
    for server in config.servers {
        for tool in connect_server(server)? {
            if !names.insert(tool.spec().name) {
                return Err("duplicate MCP tool alias".into());
            }
            staged.push(tool);
        }
    }
    // Alias preflight can stop further connections early; core validates and
    // commits the complete batch only after every connection has succeeded.
    kernel.register_tools(staged).map_err(|e| e.to_string())
}

pub fn connect(config: StdioServerConfig) -> Result<Vec<Box<dyn Tool>>, String> {
    connect_server(ServerConfig::Stdio(config))
}

pub fn connect_server(config: ServerConfig) -> Result<Vec<Box<dyn Tool>>, String> {
    config.validate()?;
    let (resolved, authority) = registration::resolve(&config)?;
    let bundle = match &config {
        ServerConfig::Stdio(server) if !server.assets.is_empty() => Some(
            fliuno_sandbox::staged_files_digest(&server.assets)
                .map_err(|_| "invalid MCP bundle identity")?,
        ),
        _ => None,
    };
    let timeout = Duration::from_millis(config.timeout_ms());
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .max_blocking_threads(4)
        .enable_all()
        .build()
        .map_err(|_| "cannot start MCP runtime")?;
    let service = runtime.block_on(async {
        tokio::time::timeout(timeout, async {
            match (&config, resolved) {
                (
                    ServerConfig::Stdio(config),
                    registration::Resolved::Stdio {
                        environment,
                        executable_digest,
                    },
                ) => launch::connect(config, environment, executable_digest).await,
                (ServerConfig::Http(config), registration::Resolved::Http { url, headers }) => {
                    http::connect_with_headers(config, url, headers).await
                }
                _ => Err("MCP resolved transport mismatch".into()),
            }
        })
        .await
        .map_err(|_| "MCP initialization timed out")?
    })?;
    let host = Arc::new(Host {
        authority,
        service: Mutex::new(service),
        runtime,
        timeout,
    });
    let (tools, capabilities) = host.runtime.block_on(async {
        let service = host.service.lock().await;
        let capabilities = service
            .peer_info()
            .ok_or("MCP handshake has no peer info")?
            .capabilities
            .clone();
        let tools = if capabilities.tools.is_some() {
            tokio::time::timeout(timeout, discovery::tools(&service))
                .await
                .map_err(|_| "MCP discovery timed out")??
        } else {
            Vec::new()
        };
        Ok::<_, String>((tools, capabilities))
    })?;
    let mut registered = wrap_tools(Arc::clone(&host), config.id(), tools, bundle.as_deref())?;
    registered.extend(content::tools(host, config.id(), &capabilities));
    Ok(registered)
}

fn wrap_tools(
    host: Arc<Host>,
    server: &str,
    tools: Vec<rmcp::model::Tool>,
    bundle: Option<&str>,
) -> Result<Vec<Box<dyn Tool>>, String> {
    let mut registered: Vec<Box<dyn Tool>> = Vec::new();
    for tool in tools {
        let schema =
            serde_json::to_value(&tool.input_schema).map_err(|_| "invalid MCP tool schema")?;
        let description = tool.description.map(|s| s.into_owned()).unwrap_or_default();
        if tool.name.is_empty()
            || tool.name.chars().any(char::is_control)
            || tool.name.len() > 256
            || description.len() > 4096
            || fliuno_core::json_size(&schema).map_err(|_| "invalid MCP tool schema")? > 65_536
        {
            return Err(
                "MCP tool name, description or schema exceeds its configured size limit".into(),
            );
        }
        // HTTP/file reference resolution is disabled by Cargo features.
        let validator =
            jsonschema::validator_for(&schema).map_err(|_| "unsupported MCP input schema")?;
        registered.push(Box::new(McpTool {
            host: Arc::clone(&host),
            name: bundle.map_or_else(
                || alias(server, &tool.name),
                |bundle| bundle_alias(server, &tool.name, bundle),
            ),
            remote_name: tool.name.to_string(),
            description,
            schema,
            validator,
        }));
    }
    Ok(registered)
}

// Stable across process restarts and independent of discovery order. The hash
// disambiguates names that sanitize to the same ASCII label; registration also
// rejects duplicate aliases. This is naming, not a security primitive.
fn alias(server: &str, remote: &str) -> String {
    let hash = remote.bytes().fold(0xcbf29ce484222325_u64, |h, b| {
        (h ^ u64::from(b)).wrapping_mul(0x100000001b3)
    });
    let label: String = remote
        .chars()
        .take(24)
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    format!("mcp_{server}_{label}_{hash:016x}")
}

// Bundled server code is part of the approval authority, unlike the legacy
// display alias. A cryptographic 128-bit suffix binds its pinned files and
// remote tool name, so a changed bundle cannot take an older pending approval.
fn bundle_alias(server: &str, remote: &str, bundle: &str) -> String {
    let mut digest = Sha256::new();
    for field in [server, remote, bundle] {
        digest.update((field.len() as u64).to_le_bytes());
        digest.update(field.as_bytes());
    }
    let hash = format!("{:x}", digest.finalize());
    let label: String = remote
        .chars()
        .take(8)
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    format!("mcp_{server}_{label}_{}", &hash[..32])
}

struct McpTool {
    host: Arc<Host>,
    name: String,
    remote_name: String,
    description: String,
    schema: Value,
    validator: jsonschema::Validator,
}

impl Tool for McpTool {
    fn authority(&self) -> String {
        fliuno_core::authority_digest(
            "mcp.tool.v2",
            &serde_json::json!({
                "host":self.host.authority, "remoteName":self.remote_name,
                "description":self.description, "inputSchema":self.schema,
            }),
        )
    }
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.name.clone(),
            description: self.description.clone(),
            effect: ToolEffect::External,
            input_schema: self.schema.clone(),
        }
    }

    fn validate(&self, arguments: &Value) -> Result<(), String> {
        if !arguments.is_object() || !self.validator.is_valid(arguments) {
            return Err("MCP arguments do not match the registered schema".into());
        }
        Ok(())
    }

    fn execute(
        &self,
        _context: &ToolContext<'_>,
        call: &ToolCall,
        cancellation: &CancellationToken,
    ) -> Result<ToolOutput, ToolError> {
        self.validate(&call.arguments)?;
        let arguments = call
            .arguments
            .as_object()
            .ok_or("MCP arguments must be an object")?
            .clone();
        let name = self.remote_name.clone();
        let response = self.host.request(cancellation, move |service| {
            Box::pin(async move {
                // The high-level SDK helper drives MRTR rounds itself. Keep one
                // request so continuations require their own durable host state.
                Ok(
                    match service
                        .call_tool_once(CallToolRequestParams::new(name).with_arguments(arguments))
                        .await?
                    {
                        CallToolResponse::Complete(result)
                            if host::is_complete(&result.result_type) =>
                        {
                            Some(result)
                        }
                        _ => None,
                    },
                )
            })
        })?;
        let output = serde_json::to_value(response).map_err(|_| "cannot encode MCP output")?;
        if fliuno_core::json_size(&output).map_err(|_| "cannot encode MCP output")? > 1024 * 1024 {
            return Err(ToolError::OutcomeUnknown("MCP output exceeds 1 MiB".into()));
        }
        // A protocol success carrying isError=true is still a known tool result.
        Ok(ToolOutput {
            output,
            artifacts: vec![],
        })
    }
}

#[cfg(test)]
mod tests;
