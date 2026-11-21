//! Explicit, approval-gated controls for remote MCP Tasks.
use crate::host::{Host, McpService};
use crate::tool_schema::ToolSchema;
use fliuno_core::{
    CancellationToken, McpTaskRecord, McpTaskStore, Tool, ToolContext, ToolEffect, ToolError,
    ToolOutput, ToolSpec, authority_digest, json_size,
};
use fliuno_protocol::ToolCall;
use rmcp::{
    ServiceError,
    model::{
        CallToolResult, CancelTaskParams, GetTaskParams, GetTaskResult, ResultType, Task,
        TaskPayload, UpdateTaskParams,
    },
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex, OnceLock},
    time::Duration,
};

mod expiry;
pub(super) mod legacy;
pub(crate) mod legacy_input;
mod wait;
pub(crate) mod watch;

const TASK_ID_MAX_BYTES: usize = 512;
const INPUT_REQUESTS_MAX: usize = 16;
const INPUT_RESPONSES_MAX_BYTES: usize = 256 * 1024;
const RESULT_MAX_BYTES: usize = 1024 * 1024;
const MAX_BOUND_TASKS: usize = 4096;
const MAX_ANSWERED_INPUTS: usize = 64;

#[derive(Clone, PartialEq, Eq)]
pub(super) struct Scope {
    workspace: String,
    session: String,
}

impl From<&ToolContext<'_>> for Scope {
    fn from(context: &ToolContext<'_>) -> Self {
        Self {
            workspace: context.workspace.id.clone(),
            session: context.session_id.to_owned(),
        }
    }
}

pub(super) type OutputContract = (Value, ToolSchema);

pub(super) struct Origin {
    scope: Scope,
    task_id: String,
    call_id: String,
}

impl Origin {
    pub fn new(context: &ToolContext<'_>, call_id: &str) -> Self {
        Self {
            scope: Scope::from(context),
            task_id: context.task_id.into(),
            call_id: call_id.into(),
        }
    }
}

#[derive(Default)]
pub(crate) struct Bindings {
    cached: Mutex<BTreeMap<String, Arc<Binding>>>,
    storage: OnceLock<Arc<Storage>>,
}

struct Storage {
    store: Arc<McpTaskStore>,
    server: String,
}

struct Binding {
    scope: Scope,
    created_at: String,
    output_schema: Option<ToolSchema>,
    answered_inputs: Mutex<BTreeSet<String>>,
    storage: Option<Arc<Storage>>,
}

impl Bindings {
    pub(super) fn enable_storage(&self, store: Arc<McpTaskStore>, server: String) {
        assert!(
            self.storage
                .set(Arc::new(Storage { store, server }))
                .is_ok()
        );
    }

    pub(super) fn insert(
        &self,
        task: &Task,
        origin: Origin,
        output_schema: Option<OutputContract>,
    ) -> bool {
        self.insert_handle(&task.task_id, &task.created_at, origin, output_schema)
    }

    fn insert_handle(
        &self,
        task_id: &str,
        created_at: &str,
        origin: Origin,
        output_schema: Option<OutputContract>,
    ) -> bool {
        if validate_task_id(task_id).is_err() || created_at.is_empty() || created_at.len() > 128 {
            return false;
        }
        let mut bindings = self.cached.lock().expect("task bindings lock poisoned");
        // Reused IDs cannot replace their original contract. The caller closes
        // this connection when it cannot safely attach a handle.
        if bindings.len() >= MAX_BOUND_TASKS || bindings.contains_key(task_id) {
            return false;
        }
        let storage = self.storage.get();
        if let Some(storage) = storage {
            let record = McpTaskRecord {
                workspace_id: origin.scope.workspace.clone(),
                session_id: origin.scope.session.clone(),
                local_task_id: origin.task_id,
                local_call_id: origin.call_id,
                created_at: created_at.into(),
                output_schema: output_schema.as_ref().map(|(schema, _)| schema.clone()),
            };
            if !matches!(
                storage.store.insert(&storage.server, task_id, &record),
                Ok(true)
            ) {
                return false;
            }
        }
        bindings.insert(
            task_id.to_owned(),
            Arc::new(Binding {
                scope: origin.scope,
                created_at: created_at.to_owned(),
                output_schema: output_schema.map(|(_, validator)| validator),
                answered_inputs: Mutex::default(),
                storage: storage.cloned(),
            }),
        );
        true
    }

    fn lookup(&self, task_id: &str, scope: &Scope) -> Result<Option<Arc<Binding>>, ToolError> {
        let mut cached = self.cached.lock().expect("task bindings lock poisoned");
        if let Some(binding) = cached.get(task_id) {
            check_scope(&binding.scope, scope)?;
            return Ok(Some(binding.clone()));
        }
        let Some(storage) = self.storage.get() else {
            return Ok(None);
        };
        let Some(record) = storage
            .store
            .get(&storage.server, task_id)
            .map_err(|_| ToolError::Failed("cannot read private MCP task binding".into()))?
        else {
            return Ok(None);
        };
        let owner = Scope {
            workspace: record.workspace_id,
            session: record.session_id,
        };
        check_scope(&owner, scope)?;
        let output_schema = record
            .output_schema
            .as_ref()
            .map(|schema| ToolSchema::compile(schema, true, false))
            .transpose()
            .map_err(|_| ToolError::Failed("cannot restore MCP task output contract".into()))?;
        let binding = Arc::new(Binding {
            scope: owner,
            created_at: record.created_at,
            output_schema,
            answered_inputs: Mutex::default(),
            storage: Some(storage.clone()),
        });
        if cached.len() < MAX_BOUND_TASKS {
            cached.insert(task_id.into(), binding.clone());
        }
        Ok(Some(binding))
    }

    fn render_snapshot(&self, snapshot: GetTaskResult, scope: &Scope) -> Result<Value, ToolError> {
        let task = &snapshot.task.task;
        let unknown = |message: &str| ToolError::OutcomeUnknownWithOutput {
            message: message.into(),
            output: json!({"taskId":task.task_id,"status":task.status}),
        };
        let Some(binding) = self.lookup(&task.task_id, scope)? else {
            if matches!(snapshot.task.payload, TaskPayload::Completed { .. }) {
                return Err(unknown("MCP task result has no local call binding"));
            }
            return Ok(json!({"taskId":task.task_id,"status":task.status,"unbound":true}));
        };
        if binding.created_at != task.created_at {
            return Err(unknown(
                "MCP task snapshot does not match the original handle",
            ));
        }
        if let TaskPayload::Completed { result } = &snapshot.task.payload {
            binding
                .validate_completed_result(result)
                .map_err(|reason| unknown(&reason))?;
        }
        serde_json::to_value(snapshot)
            .map_err(|_| ToolError::Failed("cannot encode MCP task snapshot".into()))
    }
}

fn check_scope(owner: &Scope, scope: &Scope) -> Result<(), ToolError> {
    if owner != scope {
        return Err(ToolError::Failed(
            "MCP task belongs to a different workspace or session".into(),
        ));
    }
    Ok(())
}

impl Binding {
    fn validate_completed_result(&self, result: &rmcp::model::JsonObject) -> Result<(), String> {
        if json_size(result).map_err(|_| "cannot size MCP task result")? > RESULT_MAX_BYTES {
            return Err("MCP task result exceeds the 1 MiB result budget".into());
        }
        let decoded: CallToolResult = serde_json::from_value(Value::Object(result.clone()))
            .map_err(|_| "MCP task did not return a complete tool result")?;
        if !crate::host::is_complete(&decoded.result_type) {
            return Err("MCP task did not return a complete tool result".into());
        }
        if let Some(schema) = &self.output_schema {
            // Use the original map to distinguish an explicit JSON null from an
            // omitted structuredContent member.
            let valid = match result.get("structuredContent") {
                Some(value) => schema.is_valid(value)?,
                None => decoded.is_error == Some(true),
            };
            if !valid {
                return Err(
                    "MCP task structured output does not match the original tool schema".into(),
                );
            }
        }
        Ok(())
    }

    async fn cancel(
        &self,
        service: &McpService,
        task_id: String,
    ) -> Result<Option<Result<(), ToolError>>, ServiceError> {
        let failed = |message: &str| Ok(Some(Err(ToolError::Failed(message.into()))));
        let snapshot = match fetch(service, &task_id).await {
            Ok(Some(snapshot)) => snapshot,
            Ok(None) => {
                return failed("MCP task cancellation preflight did not match the requested task");
            }
            Err(ServiceError::McpError(_)) => {
                return failed(
                    "MCP server rejected the cancellation preflight; no cancellation request sent",
                );
            }
            Err(error) => return Err(error),
        };
        if snapshot.task.task.created_at != self.created_at {
            return failed("MCP task cancellation preflight does not match the original handle");
        }
        if snapshot.task.status().is_terminal() {
            return failed("MCP task is already terminal");
        }
        if expiry::elapsed(&snapshot.task.task.created_at, snapshot.task.task.ttl_ms) {
            return failed("MCP task TTL elapsed; no cancellation request sent");
        }
        service.cancel_task(CancelTaskParams::new(task_id)).await?;
        Ok(Some(Ok(())))
    }

    async fn update(
        &self,
        service: &McpService,
        args: UpdateArgs,
    ) -> Result<Option<Result<(), ToolError>>, ServiceError> {
        let failed = |message: String| Ok(Some(Err(ToolError::Failed(message))));
        let snapshot = match fetch(service, &args.task_id).await {
            Ok(Some(snapshot)) => snapshot,
            Ok(None) => return Ok(None),
            Err(ServiceError::McpError(_)) => {
                return failed("MCP server rejected the task input preflight read".into());
            }
            Err(error) => return Err(error),
        };
        if snapshot.task.task.created_at != self.created_at {
            return failed("MCP task input snapshot does not match the original handle".into());
        }
        if expiry::elapsed(&snapshot.task.task.created_at, snapshot.task.task.ttl_ms) {
            return failed("MCP task TTL elapsed; no input sent".into());
        }
        let TaskPayload::InputRequired { input_requests } = snapshot.task.payload else {
            return failed("MCP task is not waiting for input".into());
        };
        let args = match tokio::task::spawn_blocking(move || {
            crate::validate_input_requests(&input_requests)?;
            for (id, response) in &args.input_responses {
                let request = input_requests
                    .get(id)
                    .ok_or("MCP task response does not match a current request ID")?;
                crate::validate_form_response(request, response)?;
            }
            Ok::<_, String>(args)
        })
        .await
        {
            Ok(Ok(args)) => args,
            Ok(Err(error)) => return failed(error),
            Err(_) => return failed("MCP task input validation was interrupted".into()),
        };
        if let Some(storage) = &self.storage {
            match storage.store.reserve_inputs(
                &storage.server,
                &args.task_id,
                args.input_responses.keys().map(String::as_str),
            ) {
                Ok(true) => {}
                Ok(false) => {
                    return failed(
                        "MCP task input was already sent or exceeds the task input limit".into(),
                    );
                }
                Err(_) => return failed("cannot reserve MCP task input; no request sent".into()),
            }
        } else {
            // Host serializes the preflight and write. An acknowledgement can
            // precede the remote state change, so keys are spent before dispatch.
            let mut answered = self
                .answered_inputs
                .lock()
                .expect("MCP input lock poisoned");
            if answered.len() + args.input_responses.len() > MAX_ANSWERED_INPUTS
                || args.input_responses.keys().any(|id| answered.contains(id))
            {
                return failed(
                    "MCP task input was already sent or exceeds the task input limit".into(),
                );
            }
            answered.extend(args.input_responses.keys().cloned());
        }
        service
            .update_task(UpdateTaskParams::new(args.task_id, args.input_responses))
            .await?;
        Ok(Some(Ok(())))
    }
}

#[derive(Clone, Copy)]
enum Action {
    Get,
    Wait,
    Watch,
    Update,
    Cancel,
}

enum ParsedArgs {
    TaskId(String),
    Wait(wait::Args),
    Update(UpdateArgs),
}

impl Action {
    fn method(self) -> &'static str {
        match self {
            Self::Get | Self::Wait => "tasks/get",
            Self::Watch => watch::METHOD,
            Self::Update => "tasks/update",
            Self::Cancel => "tasks/cancel",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Get => "get",
            Self::Wait => "wait",
            Self::Watch => "watch",
            Self::Update => "update",
            Self::Cancel => "cancel",
        }
    }

    fn description(self) -> &'static str {
        match self {
            Self::Get => {
                "Read one remote MCP task snapshot, including pending input requests or its final result."
            }
            Self::Wait => {
                "Wait briefly for a remote MCP task to need input or finish. Only reads task status; never sends input or cancels the remote task."
            }
            Self::Watch => {
                "Watch one bound remote MCP task briefly via status notifications, then check its current snapshot. Never sends input or cancels the task."
            }
            Self::Update => "Submit responses to pending input requests for one remote MCP task.",
            Self::Cancel => "Request cooperative cancellation of one remote MCP task.",
        }
    }
}

pub(crate) fn tools(
    host: Arc<Host>,
    server: &str,
    extension_capability: Value,
    watch: Option<watch::Endpoint>,
) -> Result<Vec<Box<dyn Tool>>, String> {
    let mut tools = Vec::with_capacity(5);
    let max_wait_ms = wait::max_wait_ms(host.timeout);
    for action in [
        Action::Get,
        Action::Wait,
        Action::Update,
        Action::Cancel,
        Action::Watch,
    ] {
        if matches!(action, Action::Watch) && watch.is_none() {
            continue;
        }
        let schema = schema(action, max_wait_ms);
        let validator =
            jsonschema::validator_for(&schema).map_err(|_| "invalid MCP task input schema")?;
        tools.push(Box::new(TaskTool {
            host: Arc::clone(&host),
            server_id: server.into(),
            name: format!("mcp_{server}_tasks_{}", action.label()),
            action,
            schema,
            validator,
            extension_capability: extension_capability.clone(),
            max_wait_ms,
            watch: watch.clone().filter(|_| matches!(action, Action::Watch)),
        }) as Box<dyn Tool>);
    }
    Ok(tools)
}

struct TaskTool {
    host: Arc<Host>,
    server_id: String,
    name: String,
    action: Action,
    schema: Value,
    validator: jsonschema::Validator,
    extension_capability: Value,
    max_wait_ms: u64,
    watch: Option<watch::Endpoint>,
}

fn schema(action: Action, max_wait_ms: u64) -> Value {
    let task_id = json!({
        "type":"string", "minLength":1, "maxLength":TASK_ID_MAX_BYTES
    });
    match action {
        Action::Get | Action::Cancel => json!({
            "type":"object", "additionalProperties":false, "required":["taskId"],
            "properties":{"taskId":task_id}
        }),
        Action::Wait | Action::Watch => json!({
            "type":"object", "additionalProperties":false,
            "required":["taskId","waitMs"],
            "properties":{"taskId":task_id,"waitMs":{"type":"integer","minimum":1,"maximum":max_wait_ms}}
        }),
        Action::Update => json!({
            "type":"object", "additionalProperties":false,
            "required":["taskId","inputResponses"],
            "properties":{
                "taskId":task_id,
                "inputResponses":{"type":"object", "minProperties":1,"maxProperties":INPUT_REQUESTS_MAX}
            }
        }),
    }
}

impl TaskTool {
    fn parse_arguments(&self, arguments: &Value) -> Result<ParsedArgs, String> {
        if !arguments.is_object() {
            return Err("MCP task arguments must be an object".into());
        }
        if !self.validator.is_valid(arguments) {
            return Err("MCP task arguments do not match the registered schema".into());
        }
        match self.action {
            Action::Get | Action::Cancel => parse_task_id(arguments).map(ParsedArgs::TaskId),
            Action::Wait | Action::Watch => {
                wait::Args::parse(arguments, self.max_wait_ms).map(ParsedArgs::Wait)
            }
            Action::Update => parse_update_args(arguments).map(ParsedArgs::Update),
        }
    }
}

impl Tool for TaskTool {
    fn authority(&self) -> String {
        if matches!(self.action, Action::Wait | Action::Watch) {
            return authority_digest(
                "mcp.task.wait.v2",
                &json!({"host":self.host.authority,"extensionCapability":self.extension_capability,
                    "method":self.action.method(),"inputSchema":self.schema}),
            );
        }
        authority_digest(
            "mcp.task.v2",
            &json!({
                "host":self.host.authority,
                "extensionCapability":self.extension_capability,
                "method":self.action.method(),
            }),
        )
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.name.clone(),
            description: self.action.description().into(),
            effect: ToolEffect::External,
            input_schema: self.schema.clone(),
        }
    }

    fn validate(&self, arguments: &Value) -> Result<(), String> {
        self.parse_arguments(arguments).map(|_| ())
    }

    fn execute(
        &self,
        context: &ToolContext<'_>,
        call: &ToolCall,
        cancellation: &CancellationToken,
    ) -> Result<ToolOutput, ToolError> {
        let parsed = self
            .parse_arguments(&call.arguments)
            .map_err(ToolError::Failed)?;
        let scope = Scope::from(context);
        let task_id = match &parsed {
            ParsedArgs::TaskId(id) => id,
            ParsedArgs::Wait(args) => &args.task_id,
            ParsedArgs::Update(args) => &args.task_id,
        };
        let binding = self.host.task_bindings.lookup(task_id, &scope)?;
        if matches!(self.action, Action::Update | Action::Cancel | Action::Watch)
            && binding.is_none()
        {
            return Err(ToolError::Failed(
                "MCP task has no local call binding".into(),
            ));
        }
        let output = match (self.action, parsed) {
            (Action::Get, ParsedArgs::TaskId(task_id)) => {
                let response = self.host.request_read(cancellation, move |service| {
                    Box::pin(async move { fetch(service, &task_id).await })
                })?;
                self.host.task_bindings.render_snapshot(response, &scope)?
            }
            (Action::Wait, ParsedArgs::Wait(args)) => {
                let observation = self.host.request_read(cancellation, move |service| {
                    Box::pin(wait::observe(service, args))
                })?;
                let snapshot = self
                    .host
                    .task_bindings
                    .render_snapshot(observation.snapshot, &scope)?;
                json!({"reason":observation.reason,"polls":observation.polls,"snapshot":snapshot})
            }
            (Action::Watch, ParsedArgs::Wait(args)) => {
                let endpoint = self
                    .watch
                    .clone()
                    .expect("task watch requires a subscription transport");
                let binding = binding.expect("a watch requires a local binding");
                let (stream, snapshot) =
                    self.host.request_read(cancellation, move |service| {
                        Box::pin(async move {
                            watch_task(service, &endpoint, args, &binding.created_at).await
                        })
                    })??;
                let snapshot = self.host.task_bindings.render_snapshot(snapshot, &scope)?;
                json!({"stream":stream,"snapshot":snapshot})
            }
            (Action::Update, ParsedArgs::Update(parsed)) => {
                let task_id = parsed.task_id.clone();
                let binding = binding.expect("mutation requires a local binding");
                self.host.request(cancellation, move |service| {
                    Box::pin(async move { binding.update(service, parsed).await })
                })??;
                json!({"taskId":task_id,"updateAccepted":true,"statusMayLag":true})
            }
            (Action::Cancel, ParsedArgs::TaskId(task_id)) => {
                let request_task_id = task_id.clone();
                let binding = binding.expect("mutation requires a local binding");
                self.host.request(cancellation, move |service| {
                    Box::pin(async move { binding.cancel(service, request_task_id).await })
                })??;
                json!({"taskId":task_id,"cancellationRequested":true,"cooperative":true})
            }
            _ => return Err(ToolError::Failed("invalid MCP task arguments".into())),
        };
        let output = json!({
            "serverId":self.server_id,
            "method":self.action.method(),
            "result":output,
        });
        if json_size(&output)
            .map_err(|_| ToolError::Failed("cannot count MCP task result".into()))?
            > RESULT_MAX_BYTES
        {
            return Err(ToolError::Failed(
                "MCP task result exceeds the 1 MiB result budget".into(),
            ));
        }
        Ok(ToolOutput {
            output,
            artifacts: Vec::new(),
        })
    }
}

async fn watch_task(
    service: &McpService,
    endpoint: &watch::Endpoint,
    args: wait::Args,
    created_at: &str,
) -> Result<Option<Result<(&'static str, GetTaskResult), ToolError>>, ServiceError> {
    let Some(snapshot) = fetch(service, &args.task_id).await? else {
        return Ok(None);
    };
    if snapshot.task.task.created_at != created_at {
        return Ok(Some(Err(ToolError::Failed(
            "MCP task watch preflight does not match the original handle".into(),
        ))));
    }
    let status = snapshot.task.status();
    if status.is_terminal() {
        return Ok(Some(Ok(("settled", snapshot))));
    }
    if expiry::elapsed(&snapshot.task.task.created_at, snapshot.task.task.ttl_ms) {
        return Ok(Some(Ok(("ttl_elapsed", snapshot))));
    }
    if status == rmcp::model::TaskStatus::InputRequired {
        return Ok(Some(Ok(("settled", snapshot))));
    }
    let deadline = tokio::time::Instant::now() + Duration::from_millis(args.wait_ms);
    let stream = endpoint
        .observe(service, &args.task_id, created_at, deadline)
        .await?;
    // Neither a notification payload nor a stream cursor enters the model
    // context. Even after EOF reconcile on the original authorized endpoint.
    Ok(fetch(service, &args.task_id)
        .await?
        .map(|snapshot| Ok((stream, snapshot))))
}

async fn fetch(service: &McpService, task_id: &str) -> Result<Option<GetTaskResult>, ServiceError> {
    let result = service.get_task(GetTaskParams::new(task_id)).await?;
    Ok(
        (result.result_type == ResultType::COMPLETE && result.task.task.task_id == task_id)
            .then_some(result),
    )
}

pub(crate) fn validate_task_id(task_id: &str) -> Result<(), String> {
    if task_id.is_empty()
        || task_id.len() > TASK_ID_MAX_BYTES
        || task_id.chars().any(char::is_control)
    {
        return Err("MCP task ID must contain 1..512 bytes without control characters".into());
    }
    Ok(())
}

fn parse_task_id(arguments: &Value) -> Result<String, String> {
    let parsed: TaskIdArgs =
        TaskIdArgs::deserialize(arguments).map_err(|_| "invalid MCP task arguments")?;
    validate_task_id(&parsed.task_id)?;
    Ok(parsed.task_id)
}

fn parse_update_args(arguments: &Value) -> Result<UpdateArgs, String> {
    let parsed: UpdateArgs =
        UpdateArgs::deserialize(arguments).map_err(|_| "invalid MCP task update arguments")?;
    validate_task_id(&parsed.task_id)?;
    if parsed.input_responses.is_empty()
        || parsed.input_responses.len() > INPUT_REQUESTS_MAX
        || parsed
            .input_responses
            .keys()
            .any(|key| key.is_empty() || key.len() > 128 || key.chars().any(char::is_control))
    {
        return Err("MCP task responses exceed their entry or key limits".into());
    }
    if json_size(&parsed.input_responses).map_err(|_| "invalid MCP task responses")?
        > INPUT_RESPONSES_MAX_BYTES
    {
        return Err("MCP task responses exceed 256 KiB".into());
    }
    Ok(parsed)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct TaskIdArgs {
    task_id: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct UpdateArgs {
    task_id: String,
    input_responses: BTreeMap<String, Value>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn update_validation_bounds_untrusted_task_responses() {
        assert!(
            parse_update_args(&json!({
                "taskId":"remote-task",
                "inputResponses":{"request-1":{"action":"accept","content":{"ok":true}}}
            }))
            .is_ok()
        );
        assert!(
            parse_update_args(&json!({
                "taskId":"remote-task",
                "inputResponses":{"request-1":{"data":"x".repeat(INPUT_RESPONSES_MAX_BYTES)}}
            }))
            .is_err()
        );
        assert!(
            parse_update_args(&json!({
                "taskId":"remote-task",
                "inputResponses":{"bad\nkey":{"value":true}}
            }))
            .is_err()
        );
    }

    #[test]
    fn task_id_validation_rejects_empty_control_and_oversized_ids() {
        for task_id in ["", "bad\nidentifier", &"x".repeat(TASK_ID_MAX_BYTES + 1)] {
            assert!(parse_task_id(&json!({"taskId":task_id})).is_err());
        }
    }
}
