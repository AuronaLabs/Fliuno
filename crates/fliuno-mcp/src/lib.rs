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
mod tasks;
pub use config::{
    HttpServerConfig, HttpTransport, Lifecycle, McpConfig, ServerConfig, StdioServerConfig,
    StdioTransport,
};
use fliuno_core::{
    CancellationToken, Kernel, Tool, ToolContext, ToolEffect, ToolError, ToolOutput, ToolSpec,
};
use fliuno_protocol::{MAX_TOOL_INPUT_BYTES, ToolCall, ToolInputRequest};
use host::Host;
pub use launch::ProcessIsolation;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, ElicitRequestParams, ElicitResult, InputRequest,
    InputRequests, InputResponses,
};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{collections::BTreeSet, sync::Arc, time::Duration};
use tokio::sync::Mutex;

const TIMEOUT: Duration = Duration::from_secs(30);

pub(crate) fn client_config() -> rmcp::model::ClientConfig {
    use rmcp::model::{
        ClientCapabilities, ClientConfig, ExtensionCapabilities, Implementation, TASKS_EXTENSION_ID,
    };

    let mut extensions = ExtensionCapabilities::new();
    extensions.insert(TASKS_EXTENSION_ID.into(), Default::default());
    let mut capabilities = ClientCapabilities::default();
    capabilities.extensions = Some(extensions);
    ClientConfig::new(
        capabilities,
        Implementation::new("fliuno", env!("CARGO_PKG_VERSION")),
    )
}

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
    let (tools, peer) = host.runtime.block_on(async {
        let service = host.service.lock().await;
        let peer = service
            .peer_info()
            .ok_or("MCP handshake has no peer info")?
            .clone();
        let tools = if peer.capabilities.tools.is_some() {
            tokio::time::timeout(timeout, discovery::tools(&service))
                .await
                .map_err(|_| "MCP discovery timed out")??
        } else {
            Vec::new()
        };
        Ok::<_, String>((tools, peer))
    })?;
    let tasks_supported = peer.capabilities.supports_tasks()
        && peer.protocol_version >= rmcp::model::ProtocolVersion::V_2026_07_28;
    let mut registered = wrap_tools(
        Arc::clone(&host),
        config.id(),
        tools,
        bundle.as_deref(),
        tasks_supported,
    )?;
    registered.extend(content::tools(Arc::clone(&host), config.id(), &peer));
    if tasks_supported {
        let extension = peer
            .capabilities
            .extensions
            .as_ref()
            .and_then(|extensions| extensions.get(rmcp::model::TASKS_EXTENSION_ID))
            .expect("tasks support was checked above");
        registered.extend(tasks::tools(
            host,
            config.id(),
            serde_json::to_value(extension).map_err(|_| "invalid MCP tasks capability")?,
        )?);
    }
    Ok(registered)
}

fn wrap_tools(
    host: Arc<Host>,
    server: &str,
    tools: Vec<rmcp::model::Tool>,
    bundle: Option<&str>,
    tasks_supported: bool,
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
            tasks_supported,
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
    tasks_supported: bool,
}

impl Tool for McpTool {
    fn authority(&self) -> String {
        let identity = serde_json::json!({
            "host":self.host.authority, "remoteName":self.remote_name,
            "description":self.description, "inputSchema":self.schema,
        });
        if self.tasks_supported {
            fliuno_core::authority_digest(
                "mcp.tool.v5",
                &serde_json::json!({"contract":identity,"tasksSupported":true}),
            )
        } else {
            fliuno_core::authority_digest("mcp.tool.v4", &identity)
        }
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

    fn validate_input_request(
        &self,
        request: &ToolInputRequest,
        request_state: Option<&str>,
    ) -> Result<(), String> {
        let parsed = parse_input_requests(&request.requests)?;
        if parsed.is_empty() && request_state.is_none() {
            return Err("MCP input-required result contains no continuation".into());
        }
        validate_input_requests(&parsed)
    }

    fn validate_input(&self, request: &ToolInputRequest, responses: &Value) -> Result<(), String> {
        let requests = parse_input_requests(&request.requests)?;
        validate_input_requests(&requests)?;
        if fliuno_core::json_size(responses).map_err(|_| "invalid MCP responses")?
            > MAX_TOOL_INPUT_BYTES
        {
            return Err("MCP input responses exceed 256 KiB".into());
        }
        let response_map = responses
            .as_object()
            .ok_or("MCP input responses must be an object")?;
        if response_map.len() != requests.len()
            || requests.keys().any(|id| !response_map.contains_key(id))
        {
            return Err("MCP input responses must match every request ID exactly".into());
        }
        for (id, input) in requests {
            let InputRequest::Elicitation(elicitation) = input else {
                return Err("only MCP form elicitation is supported".into());
            };
            let ElicitRequestParams::FormElicitationParams {
                requested_schema, ..
            } = elicitation.params
            else {
                return Err("MCP URL elicitation is not supported".into());
            };
            let result: ElicitResult = serde_json::from_value(response_map[&id].clone())
                .map_err(|_| "invalid MCP elicitation response")?;
            if result.action == rmcp::model::ElicitationAction::Accept {
                let schema = serde_json::to_value(requested_schema)
                    .map_err(|_| "invalid MCP elicitation schema")?;
                let validator = jsonschema::validator_for(&schema)
                    .map_err(|_| "invalid MCP elicitation schema")?;
                let content = result.content.as_ref().unwrap_or(&Value::Null);
                if !validator.is_valid(content) {
                    return Err(
                        "MCP elicitation response does not match its requested schema".into(),
                    );
                }
            } else if result.content.is_some() {
                return Err("non-accepted MCP elicitation cannot contain response content".into());
            }
        }
        Ok(())
    }

    fn execute(
        &self,
        _context: &ToolContext<'_>,
        call: &ToolCall,
        cancellation: &CancellationToken,
    ) -> Result<ToolOutput, ToolError> {
        self.invoke(call, None, None, cancellation)
    }

    fn resume(
        &self,
        _context: &ToolContext<'_>,
        call: &ToolCall,
        request_state: Option<&str>,
        responses: &Value,
        cancellation: &CancellationToken,
    ) -> Result<ToolOutput, ToolError> {
        self.invoke(call, request_state, Some(responses), cancellation)
    }
}

impl McpTool {
    fn invoke(
        &self,
        call: &ToolCall,
        request_state: Option<&str>,
        responses: Option<&Value>,
        cancellation: &CancellationToken,
    ) -> Result<ToolOutput, ToolError> {
        self.validate(&call.arguments)?;
        let arguments = call
            .arguments
            .as_object()
            .ok_or("MCP arguments must be an object")?
            .clone();
        let name = self.remote_name.clone();
        let tasks_supported = self.tasks_supported;
        let mut params = CallToolRequestParams::new(name).with_arguments(arguments);
        if let Some(state) = request_state {
            params = params.with_request_state(state.to_owned());
        }
        if let Some(responses) = responses {
            let parsed: InputResponses = serde_json::from_value(responses.clone())
                .map_err(|_| ToolError::Failed("invalid MCP input response map".into()))?;
            params = params.with_input_responses(parsed);
        }
        let response = self.host.request(cancellation, move |service| {
            Box::pin(async move {
                Ok(match service.call_tool_once(params).await? {
                    CallToolResponse::Complete(result)
                        if host::is_complete(&result.result_type) =>
                    {
                        Some(CallToolResponse::Complete(result))
                    }
                    CallToolResponse::Task(task)
                        if tasks_supported
                            && tasks::validate_task_id(&task.task.task_id).is_ok() =>
                    {
                        Some(CallToolResponse::Task(task))
                    }
                    CallToolResponse::InputRequired(result) => {
                        Some(CallToolResponse::InputRequired(result))
                    }
                    _ => None,
                })
            })
        })?;
        let output = match response {
            CallToolResponse::InputRequired(result) => {
                let requests = serde_json::to_value(result.input_requests.unwrap_or_default())
                    .map_err(|_| {
                        ToolError::OutcomeUnknown("cannot encode MCP input request".into())
                    })?;
                if fliuno_core::json_size(&requests).map_err(|_| {
                    ToolError::OutcomeUnknown("cannot size MCP input request".into())
                })? > MAX_TOOL_INPUT_BYTES
                    || result
                        .request_state
                        .as_ref()
                        .is_some_and(|state| state.len() > MAX_TOOL_INPUT_BYTES)
                {
                    return Err(ToolError::OutcomeUnknown(
                        "MCP input request exceeds 256 KiB".into(),
                    ));
                }
                return Err(ToolError::InputRequired {
                    requests,
                    request_state: result.request_state,
                });
            }
            CallToolResponse::Complete(result) => {
                serde_json::to_value(result).map_err(|_| "cannot encode MCP output")?
            }
            CallToolResponse::Task(task) => {
                let task_id = task.task.task_id.clone();
                let output = serde_json::to_value(task).map_err(|_| "cannot encode MCP task")?;
                if fliuno_core::json_size(&output).map_err(|_| "cannot encode MCP task")?
                    > 1024 * 1024
                {
                    return Err(ToolError::OutcomeUnknownWithOutput {
                        message: "MCP task was created but its handle exceeds the output budget; use mcp tasks/get with the task ID".into(),
                        output: serde_json::json!({"taskId":task_id}),
                    });
                }
                output
            }
            _ => unreachable!("Host::request only returns accepted MCP responses"),
        };
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

fn parse_input_requests(value: &Value) -> Result<InputRequests, String> {
    serde_json::from_value(value.clone()).map_err(|_| "invalid MCP input request map".into())
}

fn validate_input_requests(requests: &InputRequests) -> Result<(), String> {
    if requests.len() > 16
        || requests.keys().any(|id| {
            id.is_empty() || id.len() > 128 || !id.bytes().all(|byte| byte.is_ascii_graphic())
        })
    {
        return Err("MCP input request IDs exceed their bounds".into());
    }
    for request in requests.values() {
        match request {
            InputRequest::Elicitation(elicitation) => {
                if !matches!(
                    elicitation.params,
                    ElicitRequestParams::FormElicitationParams { .. }
                ) {
                    return Err("MCP URL elicitation is not supported".into());
                }
            }
            _ => return Err("MCP sampling and roots input requests are not supported".into()),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
