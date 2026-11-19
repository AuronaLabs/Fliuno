//! Explicit, approval-gated controls for remote MCP Tasks.
use crate::host::{Host, McpService};
use fliuno_core::{
    CancellationToken, Tool, ToolContext, ToolEffect, ToolError, ToolOutput, ToolSpec,
    authority_digest, json_size,
};
use fliuno_protocol::ToolCall;
use rmcp::{
    ServiceError,
    model::{CancelTaskParams, GetTaskParams, GetTaskResult, ResultType, UpdateTaskParams},
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{collections::BTreeMap, sync::Arc};

mod wait;

const TASK_ID_MAX_BYTES: usize = 512;
const INPUT_REQUESTS_MAX: usize = 16;
const INPUT_RESPONSES_MAX_BYTES: usize = 256 * 1024;
const RESULT_MAX_BYTES: usize = 1024 * 1024;

#[derive(Clone, Copy)]
enum Action {
    Get,
    Wait,
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
            Self::Update => "tasks/update",
            Self::Cancel => "tasks/cancel",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Get => "get",
            Self::Wait => "wait",
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
            Self::Update => "Submit responses to pending input requests for one remote MCP task.",
            Self::Cancel => "Request cooperative cancellation of one remote MCP task.",
        }
    }
}

pub(crate) fn tools(
    host: Arc<Host>,
    server: &str,
    extension_capability: Value,
) -> Result<Vec<Box<dyn Tool>>, String> {
    let mut tools = Vec::with_capacity(4);
    let max_wait_ms = wait::max_wait_ms(host.timeout);
    for action in [Action::Get, Action::Wait, Action::Update, Action::Cancel] {
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
        Action::Wait => json!({
            "type":"object", "additionalProperties":false,
            "required":["taskId","waitMs"],
            "properties":{"taskId":task_id,"waitMs":{"type":"integer","minimum":1,"maximum":max_wait_ms}}
        }),
        Action::Update => json!({
            "type":"object", "additionalProperties":false,
            "required":["taskId","inputResponses"],
            "properties":{
                "taskId":task_id,
                "inputResponses":{"type":"object", "maxProperties":INPUT_REQUESTS_MAX}
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
            Action::Wait => wait::Args::parse(arguments, self.max_wait_ms).map(ParsedArgs::Wait),
            Action::Update => parse_update_args(arguments).map(ParsedArgs::Update),
        }
    }
}

impl Tool for TaskTool {
    fn authority(&self) -> String {
        if matches!(self.action, Action::Wait) {
            return authority_digest(
                "mcp.task.wait.v1",
                &json!({"host":self.host.authority,"extensionCapability":self.extension_capability,
                    "method":self.action.method(),"inputSchema":self.schema}),
            );
        }
        authority_digest(
            "mcp.task.v1",
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
        _context: &ToolContext<'_>,
        call: &ToolCall,
        cancellation: &CancellationToken,
    ) -> Result<ToolOutput, ToolError> {
        let parsed = self
            .parse_arguments(&call.arguments)
            .map_err(ToolError::Failed)?;
        let output = match (self.action, parsed) {
            (Action::Get, ParsedArgs::TaskId(task_id)) => {
                let response = self.host.request_read(cancellation, move |service| {
                    Box::pin(async move { fetch(service, &task_id).await })
                })?;
                serde_json::to_value(response)
                    .map_err(|_| ToolError::Failed("cannot encode MCP task snapshot".into()))?
            }
            (Action::Wait, ParsedArgs::Wait(args)) => {
                let observation = self.host.request_read(cancellation, move |service| {
                    Box::pin(wait::observe(service, args))
                })?;
                serde_json::to_value(observation)
                    .map_err(|_| ToolError::Failed("cannot encode MCP task observation".into()))?
            }
            (Action::Update, ParsedArgs::Update(parsed)) => {
                let task_id = parsed.task_id;
                let request_task_id = task_id.clone();
                self.host.request(cancellation, move |service| {
                    Box::pin(async move {
                        service
                            .update_task(UpdateTaskParams::new(
                                request_task_id,
                                parsed.input_responses,
                            ))
                            .await
                            .map(|()| Some(()))
                    })
                })?;
                json!({"taskId":task_id,"updateAccepted":true,"statusMayLag":true})
            }
            (Action::Cancel, ParsedArgs::TaskId(task_id)) => {
                let request_task_id = task_id.clone();
                self.host.request(cancellation, move |service| {
                    Box::pin(async move {
                        service
                            .cancel_task(CancelTaskParams::new(request_task_id))
                            .await
                            .map(|()| Some(()))
                    })
                })?;
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
    if parsed.input_responses.len() > INPUT_REQUESTS_MAX
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
