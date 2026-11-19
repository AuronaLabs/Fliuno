//! Explicit checks append evidence without reopening the original run.
use super::*;

pub struct PreparedRemoteTaskCheck {
    task_id: String,
    run_id: String,
    check_id: String,
    expected: AgentRemoteTask,
    backend: Arc<dyn AgentBackend>,
}

pub struct CompletedRemoteTaskCheck {
    task_id: String,
    run_id: String,
    check_id: String,
    outcome: RemoteTaskCheckOutcome,
}

impl PreparedRemoteTaskCheck {
    pub fn task_id(&self) -> &str {
        &self.task_id
    }

    pub fn check_id(&self) -> &str {
        &self.check_id
    }

    pub fn execute(self, cancellation: &CancellationToken) -> CompletedRemoteTaskCheck {
        let observed = if cancellation.is_cancelled() {
            None
        } else {
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                self.backend.check_remote_task(&self.expected, cancellation)
            }))
            .ok()
            .flatten()
        };
        let outcome = match observed {
            None => RemoteTaskCheckOutcome::Unavailable,
            Some(observed)
                if observed.id != self.expected.id
                    || observed.context_id != self.expected.context_id =>
            {
                RemoteTaskCheckOutcome::IdentityMismatch
            }
            Some(observed)
                if self.expected.state.is_terminal() && observed.state != self.expected.state
                    || self.expected.state != AgentTaskState::Submitted
                        && observed.state == AgentTaskState::Submitted =>
            {
                RemoteTaskCheckOutcome::StateConflict
            }
            Some(observed) => RemoteTaskCheckOutcome::Observed {
                state: observed.state,
            },
        };
        CompletedRemoteTaskCheck {
            task_id: self.task_id,
            run_id: self.run_id,
            check_id: self.check_id,
            outcome,
        }
    }
}

impl Kernel {
    pub fn remote_task_check_supported(&self) -> bool {
        self.agents
            .values()
            .any(|agent| agent.supports_remote_task_check())
    }

    pub fn prepare_remote_task_check(&self, task_id: &str) -> Result<PreparedRemoteTaskCheck> {
        let task = self.get_task(task_id)?;
        let TaskBackend::ExternalAgent { agent_id } = &task.backend else {
            return Err(Error::Conflict(
                "task does not use an external agent".into(),
            ));
        };
        let run = task
            .agent_run
            .filter(|run| run.finished)
            .ok_or_else(|| Error::Conflict("agent run has not finished".into()))?;
        let expected = run
            .remote_task
            .ok_or_else(|| Error::Conflict("no remote task identity was recorded".into()))?;
        let backend = self
            .agents
            .get(agent_id)
            .filter(|agent| agent.supports_remote_task_check())
            .ok_or_else(|| Error::Conflict("agent does not support remote task checks".into()))?;
        if task.backend_authority.as_deref() != Some(backend.authority().as_str()) {
            return Err(Error::Conflict(
                "agent registration changed or lacks an authority binding".into(),
            ));
        }
        Ok(PreparedRemoteTaskCheck {
            task_id: task.id,
            run_id: run.id,
            check_id: new_id("remote_check"),
            expected,
            backend: Arc::clone(backend),
        })
    }

    pub fn record_remote_task_check(&mut self, check: CompletedRemoteTaskCheck) -> Result<()> {
        self.append_task_event(
            &check.task_id,
            EventKind::AgentRemoteTaskChecked {
                run_id: check.run_id,
                check_id: check.check_id,
                outcome: check.outcome,
            },
        )
    }
}
