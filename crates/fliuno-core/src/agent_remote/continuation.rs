//! Freeze continuation authority before scheduling; only the worker may send.
use super::*;
use crate::task::{validate_input, validate_key};

impl Kernel {
    pub fn remote_continuation_supported(&self) -> bool {
        self.agents
            .values()
            .any(|agent| agent.supports_remote_continuation())
    }

    pub fn create_remote_continuation(
        &mut self,
        source_task_id: &str,
        input: &str,
        key: &str,
    ) -> Result<(Task, bool)> {
        validate_input(input)?;
        validate_key(key)?;
        let source = self.get_task(source_task_id)?;
        if let Some(existing) = self.find_task_request(&source.session_id, key)? {
            let same_source = existing
                .remote_continuation
                .as_ref()
                .is_some_and(|continuation| continuation.source_task_id == source_task_id);
            return if same_source
                && matches!(existing.messages.first(), Some(Message::User { text }) if text == input)
            {
                Ok((existing, false))
            } else {
                Err(Error::Conflict(
                    "clientRequestId was used for a different task request".into(),
                ))
            };
        }
        let TaskBackend::ExternalAgent { agent_id } = &source.backend else {
            return Err(Error::Conflict(
                "source does not use an external agent".into(),
            ));
        };
        let expected = source
            .agent_run
            .filter(|run| run.finished && !run.outcome_unknown)
            .and_then(|run| run.remote_task)
            .filter(|remote| {
                remote.context_id.is_some()
                    && matches!(
                        remote.state,
                        AgentTaskState::InputRequired | AgentTaskState::Completed
                    )
            })
            .ok_or_else(|| {
                Error::Conflict("source has no settled remote task that can be continued".into())
            })?;
        let authority = self
            .agents
            .get(agent_id)
            .filter(|agent| agent.supports_remote_continuation())
            .map(|agent| agent.authority())
            .filter(|authority| Some(authority) == source.backend_authority.as_ref())
            .ok_or_else(|| {
                Error::Conflict("original agent registration is unavailable or changed".into())
            })?;
        let task = Task {
            id: new_id("task"),
            session_id: source.session_id,
            status: TaskStatus::Running,
            messages: vec![Message::User { text: input.into() }],
            context: None,
            pending_approval: None,
            in_flight_tool: None,
            pending_tool_input: None,
            model_outcome_unknown: false,
            active_reply_identity: None,
            context_memory_omitted_for_budget: false,
            output: None,
            model_steps: 0,
            max_model_steps: None,
            backend: source.backend,
            backend_authority: Some(authority),
            agent_run: None,
            agent_session_continuation: None,
            remote_continuation: Some(RemoteContinuation {
                source_task_id: source_task_id.into(),
                expected,
                submission_started: false,
            }),
        };
        self.commit_with_request(&task, &[EventKind::TaskStarted], &[], Some(key))?;
        Ok((task, true))
    }

    pub(crate) fn mark_remote_continuation_sending(
        &mut self,
        task_id: &str,
        run_id: &str,
    ) -> Result<()> {
        let mut task = self.get_task(task_id)?;
        if task.status != TaskStatus::Running
            || !task
                .agent_run
                .as_ref()
                .is_some_and(|run| run.id == run_id && !run.finished)
        {
            return Err(Error::Conflict(
                "remote continuation run is no longer active".into(),
            ));
        }
        let continuation = task
            .remote_continuation
            .as_mut()
            .filter(|c| !c.submission_started)
            .ok_or_else(|| {
                Error::Conflict("remote continuation cannot be submitted again".into())
            })?;
        continuation.submission_started = true;
        self.commit(
            &task,
            &[EventKind::AgentRemoteContinuationSending {
                run_id: run_id.into(),
            }],
            &[],
        )
    }
}
