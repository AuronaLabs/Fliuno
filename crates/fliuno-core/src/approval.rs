//! Approval binding and durable preparation for tool execution.
use super::*;
use serde_json::Value;
use std::panic::{AssertUnwindSafe, catch_unwind};

pub enum ApprovalOutcome {
    Finished(Task),
    Prepared {
        task: Task,
        execution: Box<PreparedToolExecution>,
    },
}

/// Owns the trusted tool and immutable inputs needed by a background worker.
pub struct PreparedToolExecution {
    task_id: String,
    session_id: String,
    call: ToolCall,
    workspace: Workspace,
    tool: Arc<dyn Tool>,
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
        catch_unwind(AssertUnwindSafe(|| {
            self.tool.execute(&context, &self.call, cancellation)
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
            TaskBackend::Model => self.prepare_model_approval(task, approval_id, decision),
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
        self.prepare_model_approval(task, approval_id, decision)
    }

    fn prepare_model_approval(
        &mut self,
        mut task: Task,
        approval_id: &str,
        decision: ApprovalDecision,
    ) -> Result<ApprovalOutcome> {
        let task_id = task.id.as_str();
        if task.status != TaskStatus::WaitingApproval {
            return Err(Error::Conflict("task is not waiting for approval".into()));
        }
        let approval = task
            .pending_approval
            .as_ref()
            .filter(|a| a.id == approval_id && a.task_id == task_id)
            .ok_or_else(|| Error::Conflict("approval does not belong to this task".into()))?
            .clone();
        let resolved = EventKind::ApprovalResolved {
            approval_id: approval_id.into(),
            decision,
        };
        task.pending_approval = None;
        if decision == ApprovalDecision::Deny {
            task.status = TaskStatus::Failed;
            task.output = Some("tool execution denied".into());
            self.commit(
                &task,
                &[
                    resolved,
                    EventKind::TaskFailed {
                        message: "tool execution denied".into(),
                    },
                ],
                &[],
            )?;
            return Ok(ApprovalOutcome::Finished(task));
        }

        let Some(tool) = self.tools.get(&approval.call.name).cloned() else {
            return Ok(ApprovalOutcome::Finished(self.fail_with_events(
                task,
                "approved tool is no longer registered".into(),
                vec![resolved],
            )?));
        };
        if approval.authority.as_deref() != Some(tool.authority().as_str()) {
            return Ok(ApprovalOutcome::Finished(self.fail_with_events(
                task,
                "approved tool registration changed or lacks an authority binding; create a new task".into(),
                vec![resolved],
            )?));
        }
        if let PolicyDecision::Deny(message) = self.policy.evaluate(&tool.spec(), &approval.call) {
            return Ok(ApprovalOutcome::Finished(self.fail_with_events(
                task,
                message,
                vec![resolved],
            )?));
        }

        let workspace = self.workspace_for_task(task_id)?;
        task.status = TaskStatus::Running;
        task.in_flight_tool = Some(approval.call.clone());
        self.commit(&task, &[resolved], &[])?;
        let execution = Box::new(PreparedToolExecution {
            task_id: task.id.clone(),
            session_id: task.session_id.clone(),
            call: approval.call,
            workspace,
            tool,
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

fn same_call(left: &ToolCall, right: &ToolCall) -> bool {
    left.id == right.id && left.name == right.name && left.arguments == right.arguments
}
