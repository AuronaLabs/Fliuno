//! Task state machine and model budget. SQL and transport live elsewhere.
use super::*;

pub(crate) mod start;

impl Kernel {
    pub(crate) fn fail(&mut self, task: Task, message: String) -> Result<Task> {
        self.fail_with_events(task, message, vec![])
    }

    pub(crate) fn fail_with_events(
        &mut self,
        mut task: Task,
        message: String,
        mut events: Vec<EventKind>,
    ) -> Result<Task> {
        task.status = TaskStatus::Failed;
        task.pending_approval = None;
        task.in_flight_tool = None;
        task.output = Some(message.clone());
        events.push(EventKind::TaskFailed { message });
        self.commit(&task, &events, &[])?;
        Ok(task)
    }

    pub(crate) fn drive(&mut self, task: Task) -> Result<Task> {
        let (task, specs) = match self.prepare_model_step(&task.id)? {
            PreparedModelStep::Ready { task, tools } => (task, tools),
            PreparedModelStep::Finished(task) => return Ok(task),
        };
        let turn = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.provider
                .next_classified(&task, &specs, &CancellationToken::new())
        }))
        .unwrap_or_else(|_| Err(ModelError::OutcomeUnknown("model provider panicked".into())));
        self.apply_model_response(task, turn)
    }

    /// Persist a model attempt and return its immutable inputs so a scheduler
    /// can invoke the provider without holding the database lock.
    pub fn prepare_model_step(&mut self, task_id: &str) -> Result<PreparedModelStep> {
        let mut task = self.get_task(task_id)?;
        if task.backend != TaskBackend::Model {
            return Err(Error::Conflict(
                "external agents require the agent execution backend".into(),
            ));
        }
        if task.status != TaskStatus::Running {
            return Ok(PreparedModelStep::Finished(task));
        }
        if task.model_outcome_unknown || task.in_flight_tool.is_some() {
            return Err(Error::Conflict(
                "task already has an in-flight operation".into(),
            ));
        }
        if model_step_budget_spent(&task) {
            return Ok(PreparedModelStep::Finished(
                self.fail(task, "model step budget exceeded".into())?,
            ));
        }
        task.model_steps += 1;
        task.model_outcome_unknown = true;
        // Persist the attempt before a paid network call. A crash must not reset
        // the budget and silently submit the same model request indefinitely.
        self.commit(
            &task,
            &[EventKind::ModelStarted {
                step: task.model_steps,
            }],
            &[],
        )?;
        Ok(PreparedModelStep::Ready {
            task,
            tools: self.tool_specs(),
        })
    }

    /// Apply a provider response after the request ran outside the database lock.
    /// A concurrent cancellation wins and late responses are discarded.
    pub fn apply_model_response(
        &mut self,
        request_task: Task,
        response: std::result::Result<ModelTurn, ModelError>,
    ) -> Result<Task> {
        let mut task = self.get_task(&request_task.id)?;
        if task.backend != TaskBackend::Model || request_task.backend != TaskBackend::Model {
            return Err(Error::Conflict(
                "model response cannot change an agent task".into(),
            ));
        }
        if task.status != TaskStatus::Running {
            return Ok(task);
        }
        if task.model_steps != request_task.model_steps
            || !task.model_outcome_unknown
            || task.in_flight_tool.is_some()
        {
            return Err(Error::Conflict(
                "model response does not match the active attempt".into(),
            ));
        }
        let turn = match response {
            Ok(turn) => turn,
            Err(ModelError::NotDispatched(message)) => {
                task.model_outcome_unknown = false;
                return self.fail(
                    task,
                    format!("model request rejected before dispatch: {message}"),
                );
            }
            Err(ModelError::CancelledBeforeDispatch) => {
                task.model_outcome_unknown = false;
                task.status = TaskStatus::Cancelled;
                task.output = None;
                self.commit(&task, &[EventKind::TaskCancelled], &[])?;
                return Ok(task);
            }
            Err(ModelError::OutcomeUnknown(message)) => {
                // A timeout may occur after a provider accepted and billed the request.
                task.status = TaskStatus::Interrupted;
                task.output = Some(format!("model outcome unknown: {message}"));
                return self.fail_interrupted_model(task);
            }
        };
        task.model_outcome_unknown = false;
        task.output = None;
        if let Some(replay) = turn.replay {
            task.messages.push(Message::ProviderReplay {
                provider: replay.provider,
                data: replay.data,
            });
        }
        let mut events: Vec<_> = turn
            .usage
            .map(|usage| EventKind::ModelUsageReported {
                step: task.model_steps,
                usage,
            })
            .into_iter()
            .collect();
        match turn.decision {
            ModelDecision::Complete(text) => {
                task.status = TaskStatus::Completed;
                task.output = Some(text.clone());
                task.messages
                    .push(Message::Assistant { text: text.clone() });
                events.push(EventKind::TaskCompleted { text });
                self.commit(&task, &events, &[])?;
                Ok(task)
            }
            ModelDecision::Call(call) => {
                if call.id.is_empty()
                    || call.id.len() > 256
                    || task.messages.iter().any(
                        |m| matches!(m, Message::ToolCall {call: previous} if previous.id==call.id),
                    )
                {
                    return self.fail_with_events(
                        task,
                        "empty, oversized or reused tool call ID".into(),
                        events,
                    );
                }
                let Some(tool) = self.tools.get(&call.name) else {
                    return self.fail_with_events(
                        task,
                        format!("unregistered tool: {}", call.name),
                        events,
                    );
                };
                if let Err(message) = tool.validate(&call.arguments) {
                    return self.fail_with_events(task, message, events);
                }
                if let PolicyDecision::Deny(message) = self.policy.evaluate(&tool.spec(), &call) {
                    return self.fail_with_events(task, message, events);
                }
                let approval = Approval {
                    id: new_id("approval"),
                    task_id: task.id.clone(),
                    call: call.clone(),
                    authority: Some(tool.authority()),
                };
                task.messages.push(Message::ToolCall { call });
                task.status = TaskStatus::WaitingApproval;
                task.pending_approval = Some(approval.clone());
                events.push(EventKind::ApprovalRequested { approval });
                self.commit(&task, &events, &[])?;
                Ok(task)
            }
        }
    }

    pub fn cancel_task(&mut self, task_id: &str) -> Result<Task> {
        let mut task = self.get_task(task_id)?;
        if task.status == TaskStatus::Cancelled {
            return Ok(task);
        }
        if task.status.is_terminal() {
            return Err(Error::Conflict("task already finished".into()));
        }
        let events = transition_to_cancelled(&mut task);
        self.commit(&task, &events, &[])?;
        self.signal_agent_cancellation(task_id);
        Ok(task)
    }

    pub(crate) fn signal_agent_cancellation(&mut self, task_id: &str) {
        if let Some(control) = self.agent_controls.remove(task_id) {
            control.cancellation.cancel();
        }
    }

    pub fn fail_task(&mut self, task_id: &str, message: String) -> Result<Task> {
        let task = self.get_task(task_id)?;
        if task.agent_run.is_some() {
            return Err(Error::Conflict(
                "started agent runs require an explicit execution outcome".into(),
            ));
        }
        if task.status != TaskStatus::Running {
            return Err(Error::Conflict(
                "only running tasks can be failed by the scheduler".into(),
            ));
        }
        self.fail(task, message)
    }

    pub fn resume_task(&mut self, task_id: &str) -> Result<Task> {
        self.resume_task_with_options(task_id, false)
    }

    pub fn resume_task_with_options(
        &mut self,
        task_id: &str,
        retry_uncertain_model_step: bool,
    ) -> Result<Task> {
        if self.get_task(task_id)?.backend != TaskBackend::Model {
            return Err(Error::Conflict(
                "external agent tasks require the background execution scheduler".into(),
            ));
        }
        let task = self.prepare_resume_task(task_id, retry_uncertain_model_step)?;
        self.drive(task)
    }

    pub fn prepare_resume_task(
        &mut self,
        task_id: &str,
        retry_uncertain_model_step: bool,
    ) -> Result<Task> {
        let mut task = self.get_task(task_id)?;
        if task.status != TaskStatus::Interrupted {
            return Err(Error::Conflict("only interrupted tasks can resume".into()));
        }
        if task.remote_continuation.is_some() {
            return Err(Error::Conflict(
                "remote continuations cannot resume; inspect submission evidence before creating another attempt".into(),
            ));
        }
        if task.agent_run.is_some() {
            return Err(Error::Conflict("started agent runs cannot be replayed; create a new task after inspecting the outcome".into()));
        }
        if let TaskBackend::ExternalAgent { agent_id } = &task.backend
            && !self.agents.contains_key(agent_id)
        {
            return Err(Error::NotFound("registered agent".into()));
        }
        if task.in_flight_tool.is_some() {
            return Err(Error::Conflict(
                "external tool outcome unknown; inspect the remote service and cancel this task"
                    .into(),
            ));
        }
        if task.model_outcome_unknown && !retry_uncertain_model_step {
            return Err(Error::Conflict(
                "model request outcome is uncertain; explicit retry consent is required".into(),
            ));
        }
        if task.backend == TaskBackend::Model && model_step_budget_spent(&task) {
            // Do not clear evidence of a possibly billed last attempt if no
            // attempt remains. Inspect or cancel the interrupted task instead.
            return Err(Error::Conflict(
                "model step budget exhausted; task cannot resume".into(),
            ));
        }
        task.model_outcome_unknown = false;
        task.status = TaskStatus::Running;
        task.output = None;
        self.commit(&task, &[EventKind::TaskResumed], &[])?;
        Ok(task)
    }

    fn fail_interrupted_model(&mut self, task: Task) -> Result<Task> {
        self.commit(
            &task,
            &[
                EventKind::TaskInterrupted,
                EventKind::ModelOutcomeUnknown {
                    step: task.model_steps,
                },
            ],
            &[],
        )?;
        Ok(task)
    }
}

fn model_step_budget_spent(task: &Task) -> bool {
    task.model_steps >= task.max_model_steps.unwrap_or(DEFAULT_TASK_MODEL_STEPS)
}

/// Prepare the same cancellation transition for single and batch callers.
pub(crate) fn transition_to_cancelled(task: &mut Task) -> Vec<EventKind> {
    debug_assert!(!task.status.is_terminal());
    task.status = TaskStatus::Cancelled;
    task.pending_approval = None;
    let mut events = vec![EventKind::TaskCancelled];
    if let Some(run) = &mut task.agent_run {
        events.extend(agent::close_approvals(run));
    }
    events
}

pub(crate) fn validate_input(input: &str) -> Result<()> {
    if input.trim().is_empty() || input.len() > 262_144 {
        return Err(Error::Invalid(
            "input must contain 1..262144 UTF-8 bytes".into(),
        ));
    }
    Ok(())
}

pub(crate) fn validate_key(key: &str) -> Result<()> {
    if key.is_empty() || key.len() > 128 || !key.bytes().all(|b| b.is_ascii_graphic()) {
        return Err(Error::Invalid(
            "clientRequestId must be 1..128 printable ASCII bytes".into(),
        ));
    }
    Ok(())
}

pub enum PreparedModelStep {
    Ready { task: Task, tools: Vec<ToolSpec> },
    Finished(Task),
}
