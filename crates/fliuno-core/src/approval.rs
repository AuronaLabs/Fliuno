//! Approval binding and durable preparation for tool execution.
use super::*;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::panic::{AssertUnwindSafe, catch_unwind};

pub enum ApprovalOutcome {
    Finished(Task),
    Prepared {
        task: Task,
        execution: Box<PreparedToolExecution>,
    },
}

pub enum ToolInputOutcome {
    Duplicate(Task),
    Prepared {
        task: Task,
        execution: Box<PreparedToolExecution>,
    },
}

struct ToolContinuation {
    request_state: Option<String>,
    responses: Value,
    round: u8,
}

/// Owns the trusted tool and immutable inputs needed by a background worker.
pub struct PreparedToolExecution {
    task_id: String,
    session_id: String,
    call: ToolCall,
    workspace: Workspace,
    tool: Arc<dyn Tool>,
    continuation: Option<ToolContinuation>,
}

impl PreparedToolExecution {
    pub(crate) fn for_agent(
        task_id: String,
        session_id: String,
        call: ToolCall,
        workspace: Workspace,
        tool: Arc<dyn Tool>,
    ) -> Self {
        Self {
            task_id,
            session_id,
            call,
            workspace,
            tool,
            continuation: None,
        }
    }
    pub fn task_id(&self) -> &str {
        &self.task_id
    }

    pub fn call(&self) -> &ToolCall {
        &self.call
    }

    pub fn execute(
        &self,
        cancellation: &CancellationToken,
    ) -> std::result::Result<ToolOutput, ToolError> {
        if cancellation.is_cancelled() {
            return Err(ToolError::Failed("tool cancelled before execution".into()));
        }
        let context = ToolContext {
            task_id: &self.task_id,
            session_id: &self.session_id,
            workspace: &self.workspace,
        };
        // The tool may have completed an external effect before unwinding.
        catch_unwind(AssertUnwindSafe(|| match &self.continuation {
            Some(continuation) => self.tool.resume(
                &context,
                &self.call,
                continuation.request_state.as_deref(),
                &continuation.responses,
                cancellation,
            ),
            None => self.tool.execute(&context, &self.call, cancellation),
        }))
        .unwrap_or_else(|_| Err(ToolError::OutcomeUnknown("tool execution panicked".into())))
    }
}

impl Kernel {
    pub fn respond_approval(
        &mut self,
        task_id: &str,
        approval_id: &str,
        decision: ApprovalDecision,
    ) -> Result<Task> {
        let task = self.resolve_approval(task_id, approval_id, decision)?;
        if task.status == TaskStatus::Running {
            self.drive(task)
        } else {
            Ok(task)
        }
    }

    /// Dispatch one task snapshot to the matching approval handler.
    pub fn prepare_approval_response(
        &mut self,
        task_id: &str,
        approval_id: &str,
        decision: ApprovalDecision,
    ) -> Result<ApprovalOutcome> {
        let task = self.get_task(task_id)?;
        match task.backend {
            TaskBackend::Model => self.prepare_model_approval(task, approval_id, decision, None),
            TaskBackend::ExternalAgent { .. } => self
                .respond_agent_approval_for_task(task, approval_id, decision)
                .map(ApprovalOutcome::Finished),
        }
    }

    /// Resolve a model approval and persist an execution identity before returning
    /// owned inputs to a scheduler. No tool code runs while the DB is locked.
    pub fn prepare_approval(
        &mut self,
        task_id: &str,
        approval_id: &str,
        decision: ApprovalDecision,
    ) -> Result<ApprovalOutcome> {
        let task = self.get_task(task_id)?;
        self.prepare_model_approval(task, approval_id, decision, None)
    }

    pub(crate) fn prepare_model_approval(
        &mut self,
        mut task: Task,
        approval_id: &str,
        decision: ApprovalDecision,
        resume: Option<&ApprovalResumeParams>,
    ) -> Result<ApprovalOutcome> {
        let task_id = task.id.as_str();
        if task.status != TaskStatus::WaitingApproval {
            return Err(Error::Conflict("task is not waiting for approval".into()));
        }
        let approval = task
            .pending_approval
            .take()
            .filter(|a| a.id == approval_id && a.task_id == task_id)
            .ok_or_else(|| Error::Conflict("approval does not belong to this task".into()))?;
        let resolved = EventKind::ApprovalResolved {
            approval_id: approval_id.into(),
            decision,
        };
        let failure = if decision == ApprovalDecision::Deny {
            Some("tool execution denied".into())
        } else {
            match self.tools.get(&approval.call.name) {
                None => Some("approved tool is no longer registered".into()),
                Some(tool) if approval.authority.as_deref() != Some(tool.authority().as_str()) => {
                    Some("approved tool registration changed or lacks an authority binding; create a new task".into())
                }
                Some(tool) => match self.policy.evaluate(&tool.spec(), &approval.call) {
                    PolicyDecision::Deny(message) => Some(message),
                    _ => None,
                },
            }
        };
        if let Some(message) = failure {
            task.status = TaskStatus::Failed;
            task.in_flight_tool = None;
            task.output = Some(message.clone());
            self.commit_with_bindings(
                &task,
                &[resolved, EventKind::TaskFailed { message }],
                &[],
                None,
                resume,
            )?;
            return Ok(ApprovalOutcome::Finished(task));
        }

        let tool = self.tools[&approval.call.name].clone();
        let workspace = self.workspace_for_task(task_id)?;
        task.status = TaskStatus::Running;
        task.in_flight_tool = Some(approval.call.clone());
        self.commit_with_bindings(&task, &[resolved], &[], None, resume)?;
        let execution = Box::new(PreparedToolExecution {
            task_id: task.id.clone(),
            session_id: task.session_id.clone(),
            call: approval.call,
            workspace,
            tool,
            continuation: None,
        });
        Ok(ApprovalOutcome::Prepared { task, execution })
    }

    /// Recheck the task and policy immediately before any tool side effect.
    pub fn begin_tool_execution(&mut self, execution: &PreparedToolExecution) -> Result<bool> {
        let mut task = self.get_task(&execution.task_id)?;
        if task.status != TaskStatus::Running
            || !task
                .in_flight_tool
                .as_ref()
                .is_some_and(|call| same_call(call, &execution.call))
        {
            return Ok(false);
        }
        if let PolicyDecision::Deny(message) = self
            .policy
            .evaluate(&execution.tool.spec(), &execution.call)
        {
            self.fail(task, message)?;
            return Ok(false);
        }
        self.commit(
            &task,
            &[EventKind::ToolStarted {
                call: execution.call.clone(),
            }],
            &[],
        )?;
        task.in_flight_tool = Some(execution.call.clone());
        Ok(true)
    }

    /// Validate a user response and prepare the same approved tool call for continuation.
    pub fn prepare_tool_input_response(
        &mut self,
        request: &ToolInputRespondParams,
    ) -> Result<ToolInputOutcome> {
        crate::task::validate_key(&request.request_id)?;
        validate_identifier(&request.call_id, 256, "call ID")?;
        let response_bytes = serde_json::to_vec(&request.responses)?;
        if response_bytes.len() > MAX_TOOL_INPUT_BYTES {
            return Err(Error::Invalid("tool input responses exceed 256 KiB".into()));
        }
        let response_sha256 = format!("{:x}", Sha256::digest(&response_bytes));
        let task = self.get_task(&request.task_id)?;
        let saved = self.tool_input_record(&request.task_id)?;
        if saved.last_request_id.as_deref() == Some(&request.request_id) {
            if saved.last_response_sha256.as_deref() == Some(&response_sha256)
                && saved.call_id == request.call_id
            {
                return Ok(ToolInputOutcome::Duplicate(task));
            }
            return Err(Error::Conflict(
                "tool input request ID was already used with a different response".into(),
            ));
        }
        if task.status != TaskStatus::WaitingInput
            || task.pending_tool_input.as_ref().is_none_or(|pending| {
                pending.call_id != request.call_id || pending.round != saved.round
            })
            || saved.call_id != request.call_id
            || task
                .in_flight_tool
                .as_ref()
                .is_none_or(|call| call.id != request.call_id)
            || saved.request_state.is_none()
                && task
                    .pending_tool_input
                    .as_ref()
                    .is_none_or(|pending| pending.requests.is_null())
        {
            return Err(Error::Conflict(
                "task is not waiting for this tool input".into(),
            ));
        }
        let call = task.in_flight_tool.clone().expect("checked in-flight call");
        let tool = self
            .tools
            .get(&call.name)
            .cloned()
            .ok_or_else(|| Error::Conflict("approved tool is no longer registered".into()))?;
        if tool.authority() != saved.authority {
            return Err(Error::Conflict(
                "approved tool registration changed while waiting for input".into(),
            ));
        }
        if let Err(message) = tool.validate_input(
            task.pending_tool_input
                .as_ref()
                .expect("checked pending input"),
            &request.responses,
        ) {
            return Err(Error::Invalid(message));
        }
        if tool.validate(&call.arguments).is_err()
            || matches!(
                self.policy.evaluate(&tool.spec(), &call),
                PolicyDecision::Deny(_)
            )
        {
            return Err(Error::Conflict(
                "tool permissions changed while waiting for input".into(),
            ));
        }
        let workspace = self.workspace_for_task(&task.id)?;
        let mut task = task;
        task.status = TaskStatus::Running;
        task.pending_tool_input = None;
        let mutation = ToolInputMutation::Resume {
            call_id: &request.call_id,
            request_id: &request.request_id,
            response_sha256: &response_sha256,
        };
        self.commit_tool_input_transition(
            &task,
            &[EventKind::ToolInputResolved {
                call_id: request.call_id.clone(),
                request_id: request.request_id.clone(),
            }],
            mutation,
        )?;
        let execution = Box::new(PreparedToolExecution {
            task_id: task.id.clone(),
            session_id: task.session_id.clone(),
            call,
            workspace,
            tool,
            continuation: Some(ToolContinuation {
                request_state: saved.request_state,
                responses: request.responses.clone(),
                round: saved.round,
            }),
        });
        Ok(ToolInputOutcome::Prepared { task, execution })
    }

    /// Apply a worker result only if its durable execution identity is current.
    /// Cancellation and recovery therefore win over a late tool response.
    pub fn apply_tool_response(
        &mut self,
        execution: &PreparedToolExecution,
        response: std::result::Result<ToolOutput, ToolError>,
    ) -> Result<Task> {
        let mut task = self.get_task(&execution.task_id)?;
        if task.status == TaskStatus::Cancelled
            && task
                .in_flight_tool
                .as_ref()
                .is_some_and(|call| same_call(call, &execution.call))
            && let Err(ToolError::OutcomeUnknownWithOutput { message, output }) = &response
        {
            let message: String = message.chars().take(1024).collect();
            let output = if json_size(output)? <= 1024 * 1024 {
                Some(output.clone())
            } else {
                None
            };
            self.commit(
                &task,
                &[EventKind::ToolOutcomeUnknown {
                    call_id: execution.call.id.clone(),
                    message,
                    output,
                }],
                &[],
            )?;
            return Ok(task);
        }
        if task.status != TaskStatus::Running
            || !task
                .in_flight_tool
                .as_ref()
                .is_some_and(|call| same_call(call, &execution.call))
        {
            return Ok(task);
        }
        let prepared = match response {
            Ok(prepared) => prepared,
            Err(ToolError::OutcomeUnknown(message)) => {
                return self.record_unknown_tool(task, execution, message, None);
            }
            Err(ToolError::OutcomeUnknownWithOutput { message, output }) => {
                return self.record_unknown_tool(task, execution, message, Some(output));
            }
            Err(ToolError::InputRequired {
                requests,
                request_state,
            }) => {
                return self.pause_tool_for_input(task, execution, requests, request_state);
            }
            Err(error) => return self.fail(task, error.to_string()),
        };

        task.messages.push(Message::ToolResult {
            call_id: execution.call.id.clone(),
            output: prepared.output.clone(),
        });
        task.status = TaskStatus::Running;
        task.in_flight_tool = None;
        let mut events = prepared
            .artifacts
            .iter()
            .map(|artifact| EventKind::ArtifactCreated {
                artifact_id: artifact.id.clone(),
            })
            .collect::<Vec<_>>();
        events.push(EventKind::ToolCompleted {
            call_id: execution.call.id.clone(),
            output: prepared.output,
        });
        self.commit(&task, &events, &prepared.artifacts)?;
        Ok(task)
    }

    fn pause_tool_for_input(
        &mut self,
        mut task: Task,
        execution: &PreparedToolExecution,
        requests: Value,
        request_state: Option<String>,
    ) -> Result<Task> {
        let request_bytes = serde_json::to_vec(&requests)?;
        let state_bytes = request_state.as_ref().map_or(0, String::len);
        let round = execution
            .continuation
            .as_ref()
            .map_or(0, |continuation| continuation.round)
            .saturating_add(1);
        let pending = ToolInputRequest {
            call_id: execution.call.id.clone(),
            requests,
            round,
        };
        let valid = round <= MAX_TOOL_INPUT_ROUNDS
            && request_bytes.len() <= MAX_TOOL_INPUT_BYTES
            && state_bytes <= MAX_TOOL_INPUT_BYTES
            && (request_state.is_some() || !pending.requests.is_null())
            && execution
                .tool
                .validate_input_request(&pending, request_state.as_deref())
                .is_ok();
        if !valid {
            return self.record_unknown_tool(
                task,
                execution,
                "tool requested unsupported, oversized or excessive additional input".into(),
                None,
            );
        }
        task.status = TaskStatus::WaitingInput;
        task.pending_tool_input = Some(pending.clone());
        self.commit_tool_input_transition(
            &task,
            &[EventKind::ToolInputRequired {
                call_id: pending.call_id.clone(),
                requests: pending.requests.clone(),
                round,
            }],
            ToolInputMutation::Suspend {
                call_id: &execution.call.id,
                authority: &execution.tool.authority(),
                request_state: request_state.as_deref(),
                round,
            },
        )?;
        Ok(task)
    }

    fn record_unknown_tool(
        &mut self,
        mut task: Task,
        execution: &PreparedToolExecution,
        message: String,
        output: Option<Value>,
    ) -> Result<Task> {
        // A failed/oversized report still leaves the operation uncertain; never
        // clear its durable identity merely because optional evidence was lost.
        let output = match output {
            Some(output) if json_size(&output)? <= 1024 * 1024 => Some(output),
            _ => None,
        };
        let message: String = message.chars().take(1024).collect();
        task.status = TaskStatus::Interrupted;
        task.output = Some(format!("tool execution result is unknown: {message}"));
        self.commit(
            &task,
            &[
                EventKind::ToolOutcomeUnknown {
                    call_id: execution.call.id.clone(),
                    message,
                    output,
                },
                EventKind::TaskInterrupted,
            ],
            &[],
        )?;
        Ok(task)
    }

    /// Synchronous compatibility API for embedded callers and core tests.
    pub fn resolve_approval(
        &mut self,
        task_id: &str,
        approval_id: &str,
        decision: ApprovalDecision,
    ) -> Result<Task> {
        match self.prepare_approval(task_id, approval_id, decision)? {
            ApprovalOutcome::Finished(task) => Ok(task),
            ApprovalOutcome::Prepared { task, execution } => {
                let cancellation = CancellationToken::new();
                if !self.begin_tool_execution(&execution)? {
                    return self.get_task(&task.id);
                }
                let response = execution.execute(&cancellation);
                self.apply_tool_response(&execution, response)
            }
        }
    }
}

#[cfg(test)]
mod tool_input_tests;

fn validate_identifier(value: &str, max_bytes: usize, label: &str) -> Result<()> {
    if value.is_empty() || value.len() > max_bytes || !value.bytes().all(|b| b.is_ascii_graphic()) {
        return Err(Error::Invalid(format!("invalid {label}")));
    }
    Ok(())
}

fn same_call(left: &ToolCall, right: &ToolCall) -> bool {
    left.id == right.id && left.name == right.name && left.arguments == right.arguments
}
