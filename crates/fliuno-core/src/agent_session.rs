//! Bind an explicit next ACP turn to its predecessor and registration.
use super::*;
use crate::task::{validate_input, validate_key};

impl Kernel {
    pub fn agent_session_continuation_supported(&self) -> bool {
        self.agents
            .values()
            .any(|agent| agent.supports_agent_session_continuation())
    }

    pub fn create_agent_session_continuation(
        &mut self,
        source_task_id: &str,
        input: &str,
        key: &str,
    ) -> Result<(Task, bool)> {
        validate_input(input)?;
        validate_key(key)?;
        // A resume prompt carries only the input and a bounded session ID.
        // Leave enough room for the escaped ID, text block and RPC envelope.
        if json_size(&input)? > 1024 * 1024 - 4096 {
            return Err(Error::Invalid(
                "encoded session prompt exceeds the wire budget".into(),
            ));
        }
        let source = self.get_task(source_task_id)?;
        if let Some(existing) = self.find_task_request(&source.session_id, key)? {
            let same_source = existing
                .agent_session_continuation
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
        let agent_session_id = source
            .agent_run
            .filter(|run| {
                source.status == TaskStatus::Completed
                    && run.finished
                    && !run.outcome_unknown
                    && run.stop_reason == Some(AgentStopReason::EndTurn)
            })
            .and_then(|run| run.session_id)
            .ok_or_else(|| Error::Conflict("source has no settled agent session".into()))?;
        let authority = self
            .agents
            .get(agent_id)
            .filter(|agent| agent.supports_agent_session_continuation())
            .map(|agent| agent.authority())
            .filter(|authority| Some(authority) == source.backend_authority.as_ref())
            .ok_or_else(|| {
                Error::Conflict(
                    "registration does not support this session continuation or changed".into(),
                )
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
            context_memory_omitted_for_budget: false,
            output: None,
            model_steps: 0,
            max_model_steps: None,
            backend: source.backend,
            backend_authority: Some(authority),
            agent_run: None,
            remote_continuation: None,
            agent_session_continuation: Some(AgentSessionContinuation {
                source_task_id: source_task_id.into(),
                agent_session_id,
                submission_started: false,
            }),
        };
        self.commit_with_request(&task, &[EventKind::TaskStarted], &[], Some(key))?;
        Ok((task, true))
    }
}
