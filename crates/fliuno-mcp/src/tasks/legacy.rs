//! 2025-11-25 Tasks are request augmentation, with a distinct result method.
use super::legacy_input::{Owner, RELATED_TASK};
use super::{
    Host, McpService, Origin, OutputContract, RESULT_MAX_BYTES, Scope, parse_task_id,
    validate_task_id, wait,
};
use crate::legacy_wire::Negotiation;
use fliuno_core::{
    CancellationToken, Tool, ToolContext, ToolEffect, ToolError, ToolOutput, ToolSpec,
    authority_digest, json_size,
};
use fliuno_protocol::{ToolCall, ToolInputRequest};
use rmcp::{
    ServiceError,
    model::{ClientRequest, CustomRequest, ServerResult, TaskStatus},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};
use tokio::time::{Instant, sleep_until};

const REVISION: &str = "2025-11-25";

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct Task {
    task_id: String,
    status: TaskStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    status_message: Option<String>,
    created_at: String,
    last_updated_at: String,
    // Some legacy SDKs omit null fields on the wire, including unlimited TTL.
    #[serde(default)]
    ttl: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    poll_interval: Option<u64>,
}

impl Task {
    fn expired(&self) -> bool {
        super::expiry::elapsed(&self.created_at, self.ttl.as_u64())
    }

    fn decode(value: &Value) -> Option<Self> {
        // A new-generation discriminator must never be treated as a legacy reply.
        if value.get("resultType").is_some() {
            return None;
        }
        let task = Self::deserialize(value).ok()?;
        (validate_task_id(&task.task_id).is_ok()
            && (task.ttl.is_null() || task.ttl.as_u64().is_some())
            && [&task.created_at, &task.last_updated_at]
                .into_iter()
                .all(|s| !s.is_empty() && s.len() <= 128 && !s.chars().any(char::is_control)))
        .then_some(task)
    }
}

async fn request_raw(
    service: &McpService,
    method: &str,
    params: Value,
) -> Result<Option<Value>, ServiceError> {
    Ok(
        match service
            .send_request(ClientRequest::CustomRequest(CustomRequest::new(
                method,
                Some(params),
            )))
            .await?
        {
            ServerResult::CustomResult(result) => Some(result.0),
            _ => None,
        },
    )
}

async fn fetch(
    service: &McpService,
    method: &str,
    task_id: &str,
) -> Result<Option<Task>, ServiceError> {
    Ok(request_raw(service, method, json!({"taskId":task_id}))
        .await?
        .as_ref()
        .and_then(Task::decode)
        .filter(|task| task.task_id == task_id))
}

pub(crate) fn start(
    host: &Host,
    context: &ToolContext<'_>,
    name: &str,
    call_id: &str,
    arguments: &Value,
    output_schema: Option<OutputContract>,
    cancellation: &CancellationToken,
) -> Result<ToolOutput, ToolError> {
    let params = json!({"name":name,"arguments":arguments,"task":{}});
    let origin = Origin::new(context, call_id);
    let bindings = Arc::clone(&host.task_bindings);
    let task = host.request(cancellation, move |service| {
        Box::pin(async move {
            let Some(result) = request_raw(service, "tools/call", params).await? else {
                return Ok(None);
            };
            if result.get("resultType").is_some() {
                return Ok(None);
            }
            let Some(task) = result.get("task").and_then(Task::decode) else {
                return Ok(None);
            };
            Ok(bindings
                .insert_handle(&task.task_id, &task.created_at, origin, output_schema)
                .then_some(task))
        })
    })?;
    output(json!({"protocolVersion":REVISION,"task":task}))
}

#[derive(Clone, Copy)]
enum Action {
    Get,
    Wait,
    Result,
    Cancel,
}

impl Action {
    fn label(self) -> &'static str {
        match self {
            Self::Get => "get",
            Self::Wait => "wait",
            Self::Result => "result",
            Self::Cancel => "cancel",
        }
    }

    fn method(self) -> &'static str {
        match self {
            Self::Get | Self::Wait => "tasks/get",
            Self::Result => "tasks/result",
            Self::Cancel => "tasks/cancel",
        }
    }
}

pub(crate) fn tools(
    host: Arc<Host>,
    server: &str,
    cancel: bool,
    negotiation: Negotiation,
) -> Vec<Box<dyn Tool>> {
    [Action::Get, Action::Wait, Action::Result, Action::Cancel]
        .into_iter()
        .filter(|action| cancel || !matches!(action, Action::Cancel))
        .map(|action| {
            Box::new(LegacyTool {
                host: Arc::clone(&host),
                name: format!("mcp_{server}_tasks_{}", action.label()),
                server: server.to_owned(),
                action,
                negotiation: negotiation.clone(),
            }) as Box<dyn Tool>
        })
        .collect()
}

struct LegacyTool {
    host: Arc<Host>,
    name: String,
    server: String,
    action: Action,
    negotiation: Negotiation,
}

impl Tool for LegacyTool {
    fn authority(&self) -> String {
        authority_digest(
            "mcp.task.2025.v2",
            &json!({"host":self.host.authority,"method":self.action.method(),"action":self.action.label()}),
        )
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.name.clone(),
            description: match self.action {
                Action::Get => "Read the current status of a legacy MCP task.",
                Action::Wait => "Wait briefly for a legacy MCP task to need input or finish. Only reads task status.",
                Action::Result => "Retrieve a finished legacy MCP task result, or relay its form to the user when it is waiting for input.",
                Action::Cancel => "Cancel a legacy MCP task belonging to this session.",
            }
            .into(),
            effect: ToolEffect::External,
            input_schema: if matches!(self.action, Action::Wait) {
                super::schema(super::Action::Wait, wait::max_wait_ms(self.host.timeout))
            } else {
                super::schema(super::Action::Get, 0)
            },
        }
    }

    fn validate(&self, args: &Value) -> Result<(), String> {
        if matches!(self.action, Action::Wait) {
            wait::Args::parse(args, wait::max_wait_ms(self.host.timeout)).map(|_| ())
        } else {
            parse_task_id(args).map(|_| ())
        }
    }

    fn validate_input_request(
        &self,
        request: &ToolInputRequest,
        state: Option<&str>,
    ) -> Result<(), String> {
        if !matches!(self.action, Action::Result) || state.is_none() {
            return Err("the legacy MCP action does not have a pending form".into());
        }
        crate::validate_input_requests(&crate::parse_input_requests(&request.requests)?)
    }

    fn validate_input(&self, request: &ToolInputRequest, responses: &Value) -> Result<(), String> {
        if !matches!(self.action, Action::Result) {
            return Err("the legacy MCP action does not accept input".into());
        }
        crate::validate_input_responses(&request.requests, responses)
    }

    fn resume(
        &self,
        context: &ToolContext<'_>,
        call: &ToolCall,
        state: Option<&str>,
        responses: &Value,
        cancellation: &CancellationToken,
    ) -> Result<ToolOutput, ToolError> {
        let missing = || {
            ToolError::OutcomeUnknown(
                "legacy MCP input no longer has its original live request".into(),
            )
        };
        let task_id = parse_task_id(&call.arguments)?;
        let token = state
            .filter(|_| matches!(self.action, Action::Result))
            .ok_or_else(missing)?
            .to_owned();
        let binding = self
            .host
            .task_bindings
            .lookup(&task_id, &Scope::from(context))?
            .ok_or_else(missing)?;
        let owner = Owner::new(context, &call.id);
        let response = responses.clone();
        let result = self
            .host
            .resume_legacy_form(cancellation, move |service| {
                Box::pin(async move {
                    Ok(service
                        .service()
                        .legacy_input
                        .resume(owner, &task_id, &token, &response)
                        .await?
                        .map(|result| {
                            result.and_then(|value| {
                                validate_result(
                                    &binding,
                                    &task_id,
                                    TaskStatus::InputRequired,
                                    value,
                                )
                            })
                        }))
                })
            })
            .map_err(|error| match error {
                ToolError::Failed(_) => missing(),
                other => other,
            })??;
        self.render(result)
    }

    fn execute(
        &self,
        context: &ToolContext<'_>,
        call: &ToolCall,
        cancellation: &CancellationToken,
    ) -> Result<ToolOutput, ToolError> {
        if matches!(self.action, Action::Wait) {
            return self.wait(context, &call.arguments, cancellation);
        }
        let task_id = parse_task_id(&call.arguments).map_err(ToolError::Failed)?;
        let binding = self
            .host
            .task_bindings
            .lookup(&task_id, &Scope::from(context))?;
        let rendered = match self.action {
            Action::Wait => unreachable!("wait was handled above"),
            Action::Get => {
                let id = task_id.clone();
                self.host.request_read(cancellation, move |service| Box::pin(async move {
                    let snapshot = fetch(service, "tasks/get", &id).await?;
                    Ok(snapshot.filter(|task| binding.as_ref().is_none_or(|entry| entry.created_at == task.created_at))
                        .map(|task| if binding.is_some() {
                            json!(task)
                        } else {
                            json!({"taskId":task.task_id,"status":task.status,"unbound":true})
                        }))
                }))?
            }
            Action::Result => {
                let binding = binding.ok_or_else(unbound)?;
                let owner = Owner::new(context, &call.id);
                let timeout = self.host.timeout.min(Duration::from_secs(120));
                self.host.request_read(cancellation, move |service| {
                    Box::pin(async move {
                        let Some(snapshot) = fetch(service, "tasks/get", &task_id).await? else {
                            return Ok(None);
                        };
                        if snapshot.created_at != binding.created_at {
                            return Ok(None);
                        }
                        if snapshot.status == TaskStatus::InputRequired {
                            if snapshot.expired() {
                                return Ok(Some(Err(ToolError::Failed(
                                    "legacy MCP task TTL elapsed; no input requested".into(),
                                ))));
                            }
                            return Ok(service
                                .service()
                                .legacy_input
                                .start(service, task_id.clone(), owner, timeout)
                                .await?
                                .map(|result| {
                                    result.and_then(|value| {
                                        validate_result(&binding, &task_id, snapshot.status, value)
                                    })
                                }));
                        }
                        if !snapshot.status.is_terminal() {
                            return Ok(Some(Err(ToolError::Failed(
                                "legacy MCP task has not reached a terminal status".into(),
                            ))));
                        }
                        let Some(result) =
                            request_raw(service, "tasks/result", json!({"taskId":task_id})).await?
                        else {
                            return Ok(None);
                        };
                        Ok(Some(validate_result(
                            &binding,
                            &task_id,
                            snapshot.status,
                            result,
                        )))
                    })
                })??
            }
            Action::Cancel => {
                let binding = binding.ok_or_else(unbound)?;
                self.host.request(cancellation, move |service| {
                    Box::pin(async move {
                        let snapshot = match fetch(service, "tasks/get", &task_id).await {
                            Ok(Some(snapshot)) => snapshot,
                            Ok(None) => return Ok(None),
                            Err(ServiceError::McpError(_)) => {
                                return Ok(Some(Err(ToolError::Failed(
                                    "legacy MCP task lookup was rejected; no cancellation request sent".into(),
                                ))));
                            }
                            Err(error) => return Err(error),
                        };
                        if snapshot.created_at != binding.created_at {
                            return Ok(None);
                        }
                        if snapshot.status.is_terminal() {
                            return Ok(Some(Err(ToolError::Failed(
                                "legacy MCP task is already terminal".into(),
                            ))));
                        }
                        if snapshot.expired() {
                            return Ok(Some(Err(ToolError::Failed(
                                "legacy MCP task TTL elapsed; no cancellation request sent".into(),
                            ))));
                        }
                        Ok(fetch(service, "tasks/cancel", &task_id)
                            .await?
                            .filter(|task| {
                                task.created_at == binding.created_at
                                    && task.status == TaskStatus::Cancelled
                            })
                            .map(|task| Ok(json!(task))))
                    })
                })??
            }
        };
        self.render(rendered)
    }
}

impl LegacyTool {
    fn render(&self, result: Value) -> Result<ToolOutput, ToolError> {
        output(
            json!({"serverId":self.server,"protocolVersion":REVISION,"method":self.action.method(),"result":result}),
        )
    }

    fn wait(
        &self,
        context: &ToolContext<'_>,
        arguments: &Value,
        cancellation: &CancellationToken,
    ) -> Result<ToolOutput, ToolError> {
        let args = wait::Args::parse(arguments, wait::max_wait_ms(self.host.timeout))
            .map_err(ToolError::Failed)?;
        let binding = self
            .host
            .task_bindings
            .lookup(&args.task_id, &Scope::from(context))?
            .ok_or_else(unbound)?;
        let negotiation = self.negotiation.clone();
        let result = self.host.request_read(cancellation, move |service| {
            Box::pin(async move { observe(service, args, &binding.created_at, negotiation).await })
        })?;
        output(json!({"serverId":self.server,"protocolVersion":REVISION,
            "method":self.action.method(),"result":result}))
    }
}

fn validate_result(
    binding: &super::Binding,
    task_id: &str,
    status: TaskStatus,
    result: Value,
) -> Result<Value, ToolError> {
    let unknown = |message: String| ToolError::OutcomeUnknownWithOutput {
        message,
        output: json!({"taskId":task_id,"status":status}),
    };
    if result["_meta"][RELATED_TASK]["taskId"].as_str() != Some(task_id)
        || result.get("resultType").is_some()
    {
        return Err(unknown(
            "legacy MCP tool result does not match its task handle".into(),
        ));
    }
    binding
        .validate_completed_result(
            result
                .as_object()
                .ok_or_else(|| unknown("invalid legacy MCP tool result".into()))?,
        )
        .map_err(unknown)?;
    Ok(result)
}

async fn observe(
    service: &McpService,
    args: wait::Args,
    created_at: &str,
    negotiation: Negotiation,
) -> Result<Option<Value>, ServiceError> {
    let deadline = Instant::now() + Duration::from_millis(args.wait_ms);
    let mut watch = negotiation.watch_status(args.task_id.clone(), created_at.to_owned());
    let mut notification = false;
    for polls in 1..=wait::MAX_POLLS {
        let Some(snapshot) = fetch(service, "tasks/get", &args.task_id)
            .await?
            .filter(|task| task.created_at == created_at)
        else {
            return Ok(None);
        };
        let reason = if snapshot.status.is_terminal() {
            "terminal"
        } else if snapshot.expired() {
            "ttl_elapsed"
        } else if snapshot.status == TaskStatus::InputRequired {
            "input_required"
        } else if polls == wait::MAX_POLLS {
            "poll_limit"
        } else {
            let interval = snapshot.poll_interval.unwrap_or(wait::DEFAULT_INTERVAL_MS);
            let next = Instant::now()
                .checked_add(Duration::from_millis(interval.max(wait::MIN_INTERVAL_MS)))
                .filter(|next| *next < deadline);
            let hint = tokio::select! {
                result = &mut watch.signal, if !notification => result.is_ok(),
                _ = sleep_until(next.unwrap_or(deadline)) => false,
            };
            if hint {
                notification = true;
            }
            // The notification is a wakeup only. Read the authoritative state
            // even when it claims the task has already finished.
            if Instant::now() < deadline && (hint || next.is_some()) {
                continue;
            }
            "wait_budget"
        };
        return Ok(Some(
            json!({"reason":reason,"polls":polls,"snapshot":snapshot,
            "notificationReceived":notification}),
        ));
    }
    unreachable!("the last poll always returns")
}

fn unbound() -> ToolError {
    ToolError::Failed("legacy MCP task has no local call binding".into())
}

fn output(value: Value) -> Result<ToolOutput, ToolError> {
    if json_size(&value)
        .map_err(|_| ToolError::OutcomeUnknown("cannot size legacy MCP task result".into()))?
        > RESULT_MAX_BYTES
    {
        return Err(ToolError::OutcomeUnknown(
            "legacy MCP task result exceeds 1 MiB".into(),
        ));
    }
    Ok(ToolOutput {
        output: value,
        artifacts: Vec::new(),
    })
}
