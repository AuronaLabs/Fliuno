//! Task state machine and model budget. SQL and transport live elsewhere.
use super::*;

pub mod compaction;
mod resume;
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
        task.pending_tool_input = None;
        task.in_flight_tool = None;
        task.output = Some(message.clone());
        events.push(EventKind::TaskFailed { message });
        self.commit(&task, &events, &[])?;
        Ok(task)
    }

    pub(crate) fn drive(&mut self, task: Task) -> Result<Task> {
        let task_id = task.id;
        loop {
            let (mut task, specs) = match self.prepare_model_step(&task_id)? {
                PreparedModelStep::Ready { task, tools } => (task, tools),
                PreparedModelStep::Finished(task) => return Ok(task),
            };
            if compaction::pending(&task) {
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    compaction::execute(self.provider.as_ref(), &task, &CancellationToken::new())
                }))
                .unwrap_or_else(|_| {
                    Err(ModelError::OutcomeUnknown(
                        "context summarizer panicked".into(),
                    ))
                });
                task = self.apply_context_compaction(task, result)?;
                if compaction::pending(&task) {
                    return Ok(task);
                }
                if task.status != TaskStatus::Running {
                    return Ok(task);
                }
                continue;
            }
            let turn = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                self.provider
                    .next_classified(&task, &specs, &CancellationToken::new())
                    .map_err(|error| error.with_input_attribution(&task, &specs))
            }))
            .unwrap_or_else(|_| Err(ModelError::OutcomeUnknown("model provider panicked".into())));
            let memory_was_omitted = task.context_memory_omitted_for_budget;
            let outputs_were_reduced = reduce_historical_tool_outputs(&task);
            task = self.apply_model_response(task, turn)?;
            let retry_without_memory =
                !memory_was_omitted && task.context_memory_omitted_for_budget;
            let retry_with_reduced_outputs =
                !outputs_were_reduced && reduce_historical_tool_outputs(&task);
            if task.status != TaskStatus::Running
                || !(retry_without_memory
                    || retry_with_reduced_outputs
                    || compaction::pending(&task))
            {
                return Ok(task);
            }
        }
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
        self.require_run_child_before_deadline(task_id)?;
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
            Err(ModelError::InputBudgetExceeded(error)) => {
                task.model_outcome_unknown = false;
                let final_message = error.terminal_message();
                let mut events: Vec<_> = error
                    .report
                    .map(|report| EventKind::ModelBudgetRejected {
                        step: task.model_steps,
                        summarization: false,
                        report: *report,
                    })
                    .into_iter()
                    .collect();
                if !model_step_budget_spent(&task)
                    && !task.context_memory_omitted_for_budget
                    && let Some(context) = &task.context
                    && !context.memories.is_empty()
                {
                    let memory_count = context.memories.len() as u32;
                    task.context_memory_omitted_for_budget = true;
                    events.push(EventKind::ContextMemoryOmittedForBudget {
                        rejected_step: task.model_steps,
                        memory_count,
                    });
                    self.commit(&task, &events, &[])?;
                    return Ok(task);
                }
                if !model_step_budget_spent(&task)
                    && let Some(context) = &mut task.context
                    && context.options.mode != ContextMode::Isolated
                    && !context.reduce_historical_tool_outputs
                {
                    let output_count = crate::context::tool_outputs::count_large(&context.turns)?;
                    if output_count > 0 {
                        context.reduce_historical_tool_outputs = true;
                        events.push(EventKind::ContextToolOutputsReduced {
                            rejected_step: task.model_steps,
                            output_count,
                        });
                        self.commit(&task, &events, &[])?;
                        return Ok(task);
                    }
                }
                if compaction::schedule(&mut task, self.provider.as_ref()) {
                    events.push(EventKind::ContextCompactionScheduled {
                        rejected_step: task.model_steps,
                    });
                    self.commit(&task, &events, &[])?;
                    return Ok(task);
                }
                return self.fail_with_events(task, final_message, events);
            }
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
        let events = self.model_usage_events(task.model_steps, &turn);
        if let Some(replay) = turn.replay {
            task.messages.push(Message::ProviderReplay {
                provider: replay.provider,
                data: replay.data,
            });
        }
        let mut events = events;
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

    pub(super) fn model_usage_events(&self, step: u32, turn: &ModelTurn) -> Vec<EventKind> {
        let mut events = turn
            .usage
            .as_ref()
            .map(|usage| EventKind::ModelUsageReported {
                step,
                usage: usage.clone(),
            })
            .into_iter()
            .collect::<Vec<_>>();
        if let Some(preflight) = &turn.preflight {
            let (usage_status, observed_input_tokens, usage_provider, undercount_tokens) =
                match turn.usage.as_ref() {
                    None => (ModelPreflightUsageStatus::Missing, None, None, None),
                    Some(usage) if usage.provider != preflight.provider => (
                        ModelPreflightUsageStatus::ProviderMismatch,
                        Some(usage.input_tokens),
                        Some(usage.provider.clone()),
                        None,
                    ),
                    Some(usage) => {
                        self.provider
                            .observe_preflight(preflight, usage.input_tokens);
                        (
                            ModelPreflightUsageStatus::Matched,
                            Some(usage.input_tokens),
                            Some(usage.provider.clone()),
                            Some(
                                usage
                                    .input_tokens
                                    .saturating_sub(preflight.estimated_input_tokens),
                            ),
                        )
                    }
                };
            events.push(EventKind::ModelPreflightObserved {
                step,
                provider: preflight.provider.clone(),
                model: preflight.model.clone(),
                count_coverage: preflight.count_coverage,
                estimated_input_tokens: preflight.estimated_input_tokens,
                safety_margin_tokens: preflight.safety_margin_tokens,
                usage_status,
                observed_input_tokens,
                usage_provider,
                margin_exceeded: undercount_tokens
                    .map(|undercount| undercount > u64::from(preflight.safety_margin_tokens)),
                undercount_tokens,
            });
        }
        events
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

fn reduce_historical_tool_outputs(task: &Task) -> bool {
    task.context
        .as_ref()
        .is_some_and(|context| context.reduce_historical_tool_outputs)
}

fn model_step_budget_spent(task: &Task) -> bool {
    remaining_model_steps(task.max_model_steps, task.model_steps) == 0
}

fn remaining_model_steps(limit: Option<u32>, attempts: u32) -> u32 {
    limit
        .unwrap_or(DEFAULT_TASK_MODEL_STEPS)
        .saturating_sub(attempts)
}

/// Prepare the same cancellation transition for single and batch callers.
pub(crate) fn transition_to_cancelled(task: &mut Task) -> Vec<EventKind> {
    debug_assert!(!task.status.is_terminal());
    task.status = TaskStatus::Cancelled;
    task.pending_approval = None;
    task.pending_tool_input = None;
    let mut events = vec![EventKind::TaskCancelled];
    if let Some(run) = &mut task.agent_run {
        if run.authentication == Some(AgentAuthenticationState::Started) {
            events.push(agent::set_authentication(
                run,
                AgentAuthenticationState::Cancelled,
            ));
        }
        events.extend(agent::close_approvals(run));
    }
    events
}

pub(crate) fn validate_input(input: &str) -> Result<()> {
    if input.trim().is_empty() || input.len() > fliuno_protocol::MAX_TASK_TEXT_BYTES {
        return Err(Error::Invalid(
            "input must contain 1..262144 UTF-8 bytes".into(),
        ));
    }
    Ok(())
}

pub(crate) fn validate_key(key: &str) -> Result<()> {
    if key.is_empty()
        || key.len() > fliuno_protocol::MAX_CLIENT_REQUEST_ID_BYTES
        || !key.bytes().all(|b| b.is_ascii_graphic())
    {
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
