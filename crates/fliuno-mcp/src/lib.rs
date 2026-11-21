//! MCP client adapter using the official Rust SDK. Servers are trusted host
//! configuration, not model-created commands. Host and isolated launch modes
//! are explicit and never fall back to one another.
mod client;
mod config;
mod content;
mod discovery;
mod egress;
mod host;
mod http;
mod launch;
mod legacy_wire;
pub mod oauth;
mod registration;
mod tasks;
mod tool_schema;
pub use config::{
    HttpServerConfig, HttpTransport, Lifecycle, McpConfig, McpOAuthConfig, ServerConfig,
    StdioServerConfig, StdioTransport,
};
pub use egress::{EgressPolicy, EgressRoute};
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
use serde::Deserialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
    time::Duration,
};
use tokio::sync::Mutex;
use tool_schema::ToolSchema;

const TIMEOUT: Duration = Duration::from_secs(30);

pub(crate) fn client_config() -> client::Client {
    use rmcp::model::{
        ClientCapabilities, ClientConfig, ExtensionCapabilities, Implementation, ProtocolVersion,
        TASKS_EXTENSION_ID,
    };

    let mut extensions = ExtensionCapabilities::new();
    extensions.insert(TASKS_EXTENSION_ID.into(), Default::default());
    let mut capabilities = ClientCapabilities::default();
    capabilities.extensions = Some(extensions);
    client::Client::new(
        ClientConfig::new(
            capabilities,
            Implementation::new("fliuno", env!("CARGO_PKG_VERSION")),
        )
        .with_protocol_version(ProtocolVersion::LATEST_WITH_INITIALIZE),
    )
}

pub fn register_config(kernel: &mut Kernel, config: McpConfig) -> Result<(), String> {
    register_config_with_oauth(kernel, config, BTreeMap::new())
}

/// Install grants supplied by the trusted host, keyed by configured server ID.
/// Grants remain outside the serialized configuration and tool arguments.
pub fn register_config_with_oauth(
    kernel: &mut Kernel,
    config: McpConfig,
    mut grants: BTreeMap<String, Arc<oauth::OAuthGrant>>,
) -> Result<(), String> {
    // Reject invalid configuration before starting any child or network IO.
    config.validate()?;
    for server in &config.servers {
        if let Some(grant) = grants.get(server.id()) {
            oauth_binding(server, grant)?;
        }
    }
    if grants
        .keys()
        .any(|id| !config.servers.iter().any(|server| server.id() == id))
    {
        return Err("MCP OAuth grant has no configured server".into());
    }
    let mut names: BTreeSet<_> = kernel.tool_specs().into_iter().map(|s| s.name).collect();
    let mut staged = Vec::new();
    let task_store = kernel
        .mcp_task_store()
        .map_err(|_| "cannot open private MCP task storage")?;
    for server in config.servers {
        let grant = grants.remove(server.id());
        for tool in connect_with_store(server, task_store.clone(), grant)? {
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
    connect_with_store(config, None, None)
}

/// Connect a configured Streamable HTTP server using a host-acquired grant.
pub fn connect_oauth(
    config: HttpServerConfig,
    grant: Arc<oauth::OAuthGrant>,
) -> Result<Vec<Box<dyn Tool>>, String> {
    connect_with_store(ServerConfig::Http(config), None, Some(grant))
}

fn oauth_binding(server: &ServerConfig, grant: &oauth::OAuthGrant) -> Result<uuid::Uuid, String> {
    let (config, url) = oauth_server(server)?;
    grant
        .binding_for_server(&url, config.egress.as_ref())
        .map_err(|error| error.to_string())
}

fn oauth_server(server: &ServerConfig) -> Result<(&HttpServerConfig, reqwest::Url), String> {
    let ServerConfig::Http(config) = server else {
        return Err("MCP OAuth requires a Streamable HTTP server".into());
    };
    if !matches!(config.transport, HttpTransport::StreamableHttp)
        || config.bearer_token_env.is_some()
        || !config.headers_env.is_empty()
    {
        return Err("MCP OAuth requires Streamable HTTP without static credentials".into());
    }
    Ok((config, http::validate(config)?))
}

fn connect_with_store(
    config: ServerConfig,
    task_store: Option<Arc<fliuno_core::McpTaskStore>>,
    grant: Option<Arc<oauth::OAuthGrant>>,
) -> Result<Vec<Box<dyn Tool>>, String> {
    config.validate()?;
    let binding = grant
        .as_ref()
        .map(|grant| oauth_binding(&config, grant))
        .transpose()?;
    let (resolved, mut authority) = registration::resolve(&config)?;
    if let Some(binding) = binding {
        authority = fliuno_core::authority_digest(
            "mcp.registration.oauth.v1",
            &serde_json::json!({"registration":authority,"grantId":binding.to_string()}),
        );
    }
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
    let (service, negotiation, task_watch) = runtime.block_on(async {
        tokio::time::timeout(timeout, async {
            match (&config, resolved) {
                (
                    ServerConfig::Stdio(config),
                    registration::Resolved::Stdio {
                        environment,
                        executable_digest,
                    },
                ) => launch::connect(config, environment, executable_digest)
                    .await
                    .map(|(service, negotiation)| (service, negotiation, None)),
                (ServerConfig::Http(config), registration::Resolved::Http { url, headers }) => {
                    http::connect_with_headers(config, url, headers, grant).await
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
        task_bindings: Default::default(),
    });
    let (tools, peer) = host.runtime.block_on(async {
        let service = host.service.lock().await;
        let peer = service
            .peer_info()
            .ok_or("MCP handshake has no peer info")?
            .clone();
        service
            .set_response_cache_config(host::cache_config(&peer.protocol_version, &host.authority))
            .await;
        let tools = if peer.capabilities.tools.is_some() {
            tokio::time::timeout(timeout, discovery::tools(&service))
                .await
                .map_err(|_| "MCP discovery timed out")??
        } else {
            Vec::new()
        };
        Ok::<_, String>((tools, peer))
    })?;
    let modern = peer.protocol_version >= rmcp::model::ProtocolVersion::V_2026_07_28;
    let tasks_supported = modern && peer.capabilities.supports_tasks();
    let legacy = if peer.protocol_version == rmcp::model::ProtocolVersion::V_2025_11_25 {
        negotiation
            .as_ref()
            .map(|state| state.registration())
            .transpose()?
            .flatten()
    } else {
        None
    };
    if (tasks_supported || legacy.as_ref().is_some_and(|entry| entry.call))
        && let Some(store) = task_store.as_ref()
    {
        let identity = fliuno_core::authority_digest(
            "mcp.tasks.binding.v1",
            &serde_json::json!({
                "host":host.authority, "protocolVersion":peer.protocol_version,
                "serverInfo":peer.server_info,
            }),
        );
        host.task_bindings
            .enable_storage(Arc::clone(store), identity);
    }
    let mut registered = wrap_tools(
        Arc::clone(&host),
        config.id(),
        tools,
        bundle.as_deref(),
        modern,
        tasks_supported,
        legacy.as_ref(),
    )?;
    registered.extend(content::tools(
        Arc::clone(&host),
        config.id(),
        &peer,
        task_store,
    ));
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
            match &config {
                ServerConfig::Http(_) => task_watch.map(tasks::watch::Endpoint::Http),
                ServerConfig::Stdio(_) => negotiation.map(tasks::watch::Endpoint::Stdio),
            },
        )?);
    } else if let Some(legacy) = legacy.filter(|entry| entry.call) {
        registered.extend(tasks::legacy::tools(
            host,
            config.id(),
            legacy.cancel,
            negotiation.expect("legacy registration requires a wire adapter"),
        ));
    }
    Ok(registered)
}

fn wrap_tools(
    host: Arc<Host>,
    server: &str,
    tools: Vec<rmcp::model::Tool>,
    bundle: Option<&str>,
    modern: bool,
    tasks_supported: bool,
    legacy: Option<&legacy_wire::Registration>,
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
        {
            return Err("MCP tool name or description exceeds its configured size limit".into());
        }
        let validator = ToolSchema::compile(&schema, modern, true)?;
        let legacy_mode = legacy.and_then(|legacy| legacy.tools.get(tool.name.as_ref()).copied());
        use legacy_wire::TaskSupport;
        if legacy_mode == Some(TaskSupport::Required) && !legacy.is_some_and(|legacy| legacy.call) {
            // This tool cannot execute in either mode under the server's declarations.
            continue;
        }
        let modes: &[bool] = match legacy_mode.filter(|_| legacy.is_some_and(|legacy| legacy.call))
        {
            Some(TaskSupport::Required) => &[true],
            Some(TaskSupport::Optional) => &[false, true],
            _ => &[false],
        };
        let output_contract = if modern || modes.contains(&true) {
            tool.output_schema
                .map(|schema| {
                    let value =
                        serde_json::to_value(schema).map_err(|_| "invalid MCP output schema")?;
                    let validator = ToolSchema::compile(&value, true, false)?;
                    Ok::<_, String>((value, validator))
                })
                .transpose()?
        } else {
            None
        };
        let base_name = bundle.map_or_else(
            || alias(server, &tool.name),
            |bundle| bundle_alias(server, &tool.name, bundle),
        );
        for &legacy_task in modes {
            let name = if legacy_task && modes.len() > 1 {
                format!("{base_name}_task")
            } else {
                base_name.clone()
            };
            registered.push(Box::new(McpTool {
                host: Arc::clone(&host),
                name,
                remote_name: tool.name.to_string(),
                description: description.clone(),
                schema: schema.clone(),
                validator: validator.clone(),
                modern,
                output_contract: if modern || legacy_task {
                    output_contract.clone()
                } else {
                    None
                },
                tasks_supported,
                legacy_task,
            }));
        }
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
    validator: ToolSchema,
    modern: bool,
    output_contract: Option<(Value, ToolSchema)>,
    tasks_supported: bool,
    legacy_task: bool,
}

impl Tool for McpTool {
    fn authority(&self) -> String {
        let identity = serde_json::json!({
            "host":self.host.authority, "remoteName":self.remote_name,
            "description":self.description, "inputSchema":self.schema,
        });
        if self.legacy_task {
            fliuno_core::authority_digest(
                "mcp.tool.2025-task.v1",
                &serde_json::json!({
                    "contract":identity,"outputSchema":self.output_contract.as_ref().map(|(schema, _)| schema),
                }),
            )
        } else if self.modern {
            fliuno_core::authority_digest(
                "mcp.tool.v6",
                &serde_json::json!({"contract":identity,"tasksSupported":self.tasks_supported,
                    "outputSchema":self.output_contract.as_ref().map(|(schema, _)| schema)}),
            )
        } else {
            fliuno_core::authority_digest("mcp.tool.v4", &identity)
        }
    }
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.name.clone(),
            description: if self.legacy_task {
                format!(
                    "{} (Returns a remote task handle; use MCP Tasks to retrieve its result.)",
                    self.description
                )
            } else {
                self.description.clone()
            },
            effect: ToolEffect::External,
            input_schema: self.schema.clone(),
        }
    }

    fn validate(&self, arguments: &Value) -> Result<(), String> {
        if self.modern
            && fliuno_core::json_size(arguments).map_err(|_| "cannot size MCP arguments")?
                > MAX_TOOL_INPUT_BYTES
        {
            return Err("MCP arguments exceed 256 KiB".into());
        }
        if !arguments.is_object() || !self.validator.is_valid(arguments)? {
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
        validate_input_responses(&request.requests, responses)
    }

    fn execute(
        &self,
        context: &ToolContext<'_>,
        call: &ToolCall,
        cancellation: &CancellationToken,
    ) -> Result<ToolOutput, ToolError> {
        self.invoke(context, call, None, None, cancellation)
    }

    fn resume(
        &self,
        context: &ToolContext<'_>,
        call: &ToolCall,
        request_state: Option<&str>,
        responses: &Value,
        cancellation: &CancellationToken,
    ) -> Result<ToolOutput, ToolError> {
        self.invoke(context, call, request_state, Some(responses), cancellation)
    }
}

impl McpTool {
    fn invoke(
        &self,
        context: &ToolContext<'_>,
        call: &ToolCall,
        request_state: Option<&str>,
        responses: Option<&Value>,
        cancellation: &CancellationToken,
    ) -> Result<ToolOutput, ToolError> {
        self.validate(&call.arguments)?;
        if self.legacy_task {
            if request_state.is_some() || responses.is_some() {
                return Err(ToolError::Failed(
                    "legacy MCP tasks cannot resume an MRTR request".into(),
                ));
            }
            return tasks::legacy::start(
                &self.host,
                context,
                &self.remote_name,
                &call.id,
                &call.arguments,
                self.output_contract.clone(),
                cancellation,
            );
        }
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
        let bindings = tasks_supported.then(|| {
            (
                Arc::clone(&self.host.task_bindings),
                tasks::Origin::new(context, &call.id),
                self.output_contract.clone(),
            )
        });
        let response = self.host.request(cancellation, move |service| {
            Box::pin(async move {
                Ok(match service.call_tool_once(params).await? {
                    CallToolResponse::Complete(result)
                        if host::is_complete(&result.result_type) =>
                    {
                        Some(CallToolResponse::Complete(result))
                    }
                    CallToolResponse::Task(task) => {
                        bindings.and_then(|(bindings, origin, schema)| {
                            bindings
                                .insert(&task.task, origin, schema)
                                .then_some(CallToolResponse::Task(task))
                        })
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
                let unknown = |message: &str| ToolError::OutcomeUnknown(message.into());
                if fliuno_core::json_size(&result).map_err(|_| unknown("cannot size MCP output"))?
                    > 1024 * 1024
                {
                    return Err(unknown("MCP output exceeds 1 MiB"));
                }
                if let Some((_, validator)) = &self.output_contract {
                    let valid = match result.structured_content.as_ref() {
                        Some(value) => {
                            validator.is_valid(value).map_err(|error| unknown(&error))?
                        }
                        None => result.is_error == Some(true),
                    };
                    if !valid {
                        return Err(unknown(
                            "MCP structured output does not match the registered schema",
                        ));
                    }
                }
                serde_json::to_value(result).map_err(|_| unknown("cannot encode MCP output"))?
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

fn validate_input_responses(requests: &Value, responses: &Value) -> Result<(), String> {
    let requests = parse_input_requests(requests)?;
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
        validate_form_response(&input, &response_map[&id])?;
    }
    Ok(())
}

fn validate_form_response(input: &InputRequest, response: &Value) -> Result<(), String> {
    let InputRequest::Elicitation(elicitation) = input else {
        return Err("only MCP form elicitation is supported".into());
    };
    let ElicitRequestParams::FormElicitationParams {
        requested_schema, ..
    } = &elicitation.params
    else {
        return Err("MCP URL elicitation is not supported".into());
    };
    let result: ElicitResult =
        ElicitResult::deserialize(response).map_err(|_| "invalid MCP elicitation response")?;
    if result.action == rmcp::model::ElicitationAction::Accept {
        let schema =
            serde_json::to_value(requested_schema).map_err(|_| "invalid MCP elicitation schema")?;
        let validator = ToolSchema::compile(&schema, true, true)?;
        if !validator.is_valid(response.get("content").unwrap_or(&Value::Null))? {
            return Err("MCP elicitation response does not match its requested schema".into());
        }
    } else if response.get("content").is_some() {
        return Err("non-accepted MCP elicitation cannot contain response content".into());
    }
    Ok(())
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
