//! Explicit, approval-gated controls for remote MCP Tasks.
use crate::host::Host;
use fliuno_core::{
    CancellationToken, Tool, ToolContext, ToolEffect, ToolError, ToolOutput, ToolSpec,
    authority_digest, json_size,
};
use fliuno_protocol::ToolCall;
use rmcp::model::{CancelTaskParams, GetTaskParams, UpdateTaskParams};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{collections::BTreeMap, sync::Arc};

const TASK_ID_MAX_BYTES: usize = 512;
const INPUT_REQUESTS_MAX: usize = 16;
const INPUT_RESPONSES_MAX_BYTES: usize = 256 * 1024;
const RESULT_MAX_BYTES: usize = 1024 * 1024;

#[derive(Clone, Copy)]
enum Action {
    Get,
    Update,
    Cancel,
}

impl Action {
    fn method(self) -> &'static str {
        match self {
            Self::Get => "tasks/get",
            Self::Update => "tasks/update",
            Self::Cancel => "tasks/cancel",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Get => "get",
            Self::Update => "update",
            Self::Cancel => "cancel",
        }
    }

    fn description(self) -> &'static str {
        match self {
            Self::Get => {
                "Read one remote MCP task snapshot, including pending input requests or its final result."
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
    let mut tools = Vec::with_capacity(3);
    for action in [Action::Get, Action::Update, Action::Cancel] {
        let schema = schema(action);
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
}

fn schema(action: Action) -> Value {
    let task_id = json!({
        "type":"string", "minLength":1, "maxLength":TASK_ID_MAX_BYTES
    });
    match action {
        Action::Get | Action::Cancel => json!({
            "type":"object", "additionalProperties":false, "required":["taskId"],
            "properties":{"taskId":task_id}
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
    fn task_id(&self, arguments: &Value) -> Result<String, String> {
        parse_task_id(arguments)
    }
}

impl Tool for TaskTool {
    fn authority(&self) -> String {
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
        if !arguments.is_object() {
            return Err("MCP task arguments must be an object".into());
        }
        if !self.validator.is_valid(arguments) {
            return Err("MCP task arguments do not match the registered schema".into());
        }
        match self.action {
            Action::Get | Action::Cancel => {
                let _ = self.task_id(arguments)?;
            }
            Action::Update => {
                let _ = parse_update_args(arguments)?;
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
        self.validate(&call.arguments)?;
        let task_id = match self.action {
            Action::Update => {
                parse_update_args(&call.arguments)
                    .map_err(ToolError::Failed)?
                    .task_id
            }
            Action::Get | Action::Cancel => self.task_id(&call.arguments)?,
        };
        let output = match self.action {
            Action::Get => {
                let response = self.host.request(cancellation, move |service| {
                    Box::pin(async move {
                        service
                            .get_task(GetTaskParams::new(task_id))
                            .await
                            .map(Some)
                    })
                })?;
                serde_json::to_value(response)
                    .map_err(|_| ToolError::Failed("cannot encode MCP task snapshot".into()))?
            }
            Action::Update => {
                let parsed = parse_update_args(&call.arguments).map_err(ToolError::Failed)?;
                self.host.request(cancellation, move |service| {
                    Box::pin(async move {
                        service
                            .update_task(UpdateTaskParams::new(
                                parsed.task_id,
                                parsed.input_responses,
                            ))
                            .await
                            .map(|()| Some(()))
                    })
                })?;
                json!({"taskId":task_id,"updateAccepted":true,"statusMayLag":true})
            }
            Action::Cancel => {
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

fn validate_task_id(task_id: &str) -> Result<(), String> {
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
        serde_json::from_value(arguments.clone()).map_err(|_| "invalid MCP task arguments")?;
    validate_task_id(&parsed.task_id)?;
    Ok(parsed.task_id)
}

fn parse_update_args(arguments: &Value) -> Result<UpdateArgs, String> {
    let parsed: UpdateArgs = serde_json::from_value(arguments.clone())
        .map_err(|_| "invalid MCP task update arguments")?;
    validate_task_id(&parsed.task_id)?;
    if parsed.input_responses.len() > INPUT_REQUESTS_MAX
        || parsed
            .input_responses
            .keys()
            .any(|key| key.is_empty() || key.len() > 128 || key.chars().any(char::is_control))
    {
        return Err("MCP task responses exceed their entry or key limits".into());
    }
    let responses = Value::Object(parsed.input_responses.clone().into_iter().collect());
    if json_size(&responses).map_err(|_| "invalid MCP task responses")? > INPUT_RESPONSES_MAX_BYTES
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
