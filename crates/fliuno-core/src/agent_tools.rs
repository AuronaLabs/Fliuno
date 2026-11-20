//! Host operations requested by an external agent use registered tools and the
//! same durable consent discipline. No SDK or filesystem IO runs in this module.
use super::*;
use agent::{MAX_PENDING, MAX_RECORDED_BYTES, now_ms};

const MAX_HOST_TOOLS: usize = 64;
const MAX_PROPOSAL_BYTES: usize = 2 * 1024 * 1024;
const MAX_OUTPUT_BYTES: usize = 1024 * 1024;

pub enum AgentToolObservation {
    Requested {
        request_id: String,
        call: ToolCall,
        preview: serde_json::Value,
        expires_at_unix_ms: u64,
    },
    Started {
        request_id: String,
    },
    Completed {
        request_id: String,
        result: std::result::Result<ToolOutput, ToolError>,
    },
    Closed {
        request_id: String,
    },
}

pub(crate) fn close_host_tools(run: &mut AgentRun) -> bool {
    let mut unknown = false;
    for operation in &mut run.host_tools {
        operation.status = match operation.status {
            AgentHostToolStatus::Running | AgentHostToolStatus::OutcomeUnknown => {
                unknown = true;
                AgentHostToolStatus::OutcomeUnknown
            }
            AgentHostToolStatus::AwaitingApproval | AgentHostToolStatus::Approved => {
                AgentHostToolStatus::Closed
            }
            status => status,
        };
    }
    unknown
}

fn require_file_tool(kernel: &Kernel, call: &ToolCall) -> Result<Arc<dyn Tool>> {
    let tool = kernel
        .tools
        .get(&call.name)
        .ok_or_else(|| Error::NotFound("registered host tool".into()))?;
    if !matches!(
        tool.spec().effect,
        ToolEffect::WorkspaceRead | ToolEffect::WorkspaceWrite
    ) {
        return Err(Error::Conflict(
            "agent host requests only support workspace tools".into(),
        ));
    }
    tool.validate(&call.arguments).map_err(Error::Invalid)?;
    if let PolicyDecision::Deny(_) = kernel.policy.evaluate(&tool.spec(), call) {
        return Err(Error::Conflict("host tool denied by current policy".into()));
    }
    Ok(Arc::clone(tool))
}

impl Kernel {
    pub fn observe_agent_tool(
        &mut self,
        task_id: &str,
        run_id: &str,
        observation: AgentToolObservation,
    ) -> Result<()> {
        let mut task = self.get_task(task_id)?;
        if !matches!(
            task.status,
            TaskStatus::Running | TaskStatus::WaitingApproval
        ) {
            return Err(Error::Conflict("agent task is no longer active".into()));
        }
        let run = task
            .agent_run
            .as_mut()
            .filter(|r| r.id == run_id && !r.finished)
            .ok_or_else(|| Error::Conflict("inactive host-tool run".into()))?;
        let mut events = Vec::new();
        let mut unsafe_result = false;
        match observation {
            AgentToolObservation::Requested {
                request_id,
                call,
                preview,
                expires_at_unix_ms,
            } => {
                let request = serde_json::json!({"call":call,"preview":preview});
                let bytes = json_size(&request)?;
                let now = now_ms()?;
                if request_id.is_empty()
                    || request_id.len() > 256
                    || call.id != request_id
                    || run.host_tools.len() >= MAX_HOST_TOOLS
                    || bytes > MAX_PROPOSAL_BYTES
                    || run.recorded_bytes + bytes as u64 > MAX_RECORDED_BYTES
                    || run.pending_approvals.len() >= MAX_PENDING
                    || run.session_id.is_none()
                    || expires_at_unix_ms <= now
                    || expires_at_unix_ms.saturating_sub(now) > 3_600_000
                    || run.host_tools.iter().any(|op| op.id == request_id)
                {
                    return Err(Error::Conflict(
                        "invalid host operation or host-tool budget exceeded".into(),
                    ));
                }
                require_file_tool(self, &call)?;
                let approval = AgentApproval {
                    id: new_id("approval"),
                    run_id: run_id.into(),
                    permission_id: request_id.clone(),
                    source: AgentApprovalSource::HostTool,
                    request,
                    allow_once_available: true,
                    expires_at_unix_ms,
                };
                let operation = AgentHostTool {
                    id: request_id,
                    call,
                    status: AgentHostToolStatus::AwaitingApproval,
                    output: None,
                    error: None,
                };
                run.host_tools.push(operation.clone());
                run.pending_approvals.push(approval.clone());
                run.recorded_bytes += bytes as u64;
                task.status = TaskStatus::WaitingApproval;
                events.push(EventKind::AgentHostToolRequested {
                    run_id: run_id.into(),
                    operation,
                });
                events.push(EventKind::AgentApprovalRequested { approval });
            }
            AgentToolObservation::Started { request_id } => {
                if self
                    .agent_controls
                    .get(task_id)
                    .is_none_or(|c| c.cancellation.is_cancelled())
                {
                    return Err(Error::Conflict(
                        "host operation was cancelled before execution".into(),
                    ));
                }
                let operation = run
                    .host_tools
                    .iter_mut()
                    .find(|op| op.id == request_id && op.status == AgentHostToolStatus::Approved)
                    .ok_or_else(|| {
                        Error::Conflict("host operation was not approved or already started".into())
                    })?;
                require_file_tool(self, &operation.call)?;
                operation.status = AgentHostToolStatus::Running;
                events.push(EventKind::AgentHostToolStarted {
                    run_id: run_id.into(),
                    request_id,
                });
            }
            AgentToolObservation::Completed { request_id, result } => {
                let operation = run
                    .host_tools
                    .iter_mut()
                    .find(|op| op.id == request_id && op.status == AgentHostToolStatus::Running)
                    .ok_or_else(|| {
                        Error::Conflict("host result does not match a running operation".into())
                    })?;
                match result {
                    Ok(output) => {
                        let bytes = json_size(&output.output)?;
                        if !output.artifacts.is_empty()
                            || bytes > MAX_OUTPUT_BYTES
                            || run.recorded_bytes + bytes as u64 > MAX_RECORDED_BYTES
                        {
                            unsafe_result = true;
                            operation.status = AgentHostToolStatus::OutcomeUnknown;
                            operation.error = Some("host result budget exceeded".into());
                        } else {
                            operation.status = AgentHostToolStatus::Succeeded;
                            operation.output = Some(output.output);
                            run.recorded_bytes += bytes as u64;
                        }
                    }
                    Err(error) => {
                        operation.status = match &error {
                            ToolError::Failed(_) => AgentHostToolStatus::Failed,
                            ToolError::InputRequired { .. } => AgentHostToolStatus::Failed,
                            ToolError::OutcomeUnknown(_)
                            | ToolError::OutcomeUnknownWithOutput { .. } => {
                                AgentHostToolStatus::OutcomeUnknown
                            }
                        };
                        unsafe_result = operation.status == AgentHostToolStatus::OutcomeUnknown;
                        let message = error.to_string();
                        operation.error = Some(message.chars().take(1024).collect());
                        if let ToolError::OutcomeUnknownWithOutput { output, .. } = error {
                            let bytes = json_size(&output)?;
                            if bytes <= MAX_OUTPUT_BYTES
                                && run.recorded_bytes + bytes as u64 <= MAX_RECORDED_BYTES
                            {
                                operation.output = Some(output);
                                run.recorded_bytes += bytes as u64;
                            }
                        }
                    }
                }
                events.push(EventKind::AgentHostToolCompleted {
                    run_id: run_id.into(),
                    request_id,
                    status: operation.status,
                    output: operation.output.clone(),
                    error: operation.error.clone(),
                });
            }
            AgentToolObservation::Closed { request_id } => {
                let Some(operation) = run.host_tools.iter_mut().find(|op| op.id == request_id)
                else {
                    return Ok(());
                };
                operation.status = match operation.status {
                    AgentHostToolStatus::AwaitingApproval | AgentHostToolStatus::Approved => {
                        AgentHostToolStatus::Closed
                    }
                    AgentHostToolStatus::Running => AgentHostToolStatus::OutcomeUnknown,
                    _ => return Ok(()),
                };
                if let Some(index) = run.pending_approvals.iter().position(|a| {
                    a.source == AgentApprovalSource::HostTool && a.permission_id == request_id
                }) {
                    let approval = run.pending_approvals.remove(index);
                    events.push(EventKind::AgentApprovalClosed {
                        run_id: run_id.into(),
                        approval_id: approval.id,
                    });
                }
                if run.pending_approvals.is_empty() {
                    task.status = TaskStatus::Running;
                }
                events.push(EventKind::AgentHostToolCompleted {
                    run_id: run_id.into(),
                    request_id,
                    status: operation.status,
                    output: None,
                    error: None,
                });
            }
        }
        self.commit(&task, &events, &[])?;
        if unsafe_result {
            return Err(Error::Conflict(
                "host-tool result is uncertain; close the agent connection".into(),
            ));
        }
        Ok(())
    }

    pub(crate) fn prepare_agent_host_command(
        &self,
        task: &mut Task,
        id: &str,
        decision: ApprovalDecision,
    ) -> Result<AgentCommand> {
        let run = task
            .agent_run
            .as_mut()
            .ok_or_else(|| Error::Conflict("missing host-tool run".into()))?;
        let operation = run
            .host_tools
            .iter_mut()
            .find(|op| op.id == id && op.status == AgentHostToolStatus::AwaitingApproval)
            .ok_or_else(|| Error::Conflict("host operation no longer awaits consent".into()))?;
        let execution = if decision == ApprovalDecision::AllowOnce {
            let tool = require_file_tool(self, &operation.call)?;
            operation.status = AgentHostToolStatus::Approved;
            Some(Box::new(PreparedToolExecution::for_agent(
                task.id.clone(),
                task.session_id.clone(),
                operation.call.clone(),
                self.workspace_for_task(&task.id)?,
                tool,
            )))
        } else {
            operation.status = AgentHostToolStatus::Denied;
            None
        };
        Ok(AgentCommand::HostTool {
            request_id: id.into(),
            execution,
        })
    }
}
