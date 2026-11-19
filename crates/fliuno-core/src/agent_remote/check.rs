//! Explicit checks append evidence without reopening the original run.
use super::*;
use std::time::Duration;

pub struct PreparedRemoteTaskCheck {
    task_id: String,
    run_id: String,
    check_id: String,
    expected: AgentRemoteTask,
    backend: Arc<dyn AgentBackend>,
    wait: Option<Duration>,
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
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| match self.wait {
                Some(wait) => self
                    .backend
                    .watch_remote_task(&self.expected, wait, cancellation),
                None => self.backend.check_remote_task(&self.expected, cancellation),
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

    pub fn remote_task_watch_supported(&self) -> bool {
        self.agents
            .values()
            .any(|agent| agent.supports_remote_task_watch())
    }

    pub fn prepare_remote_task_check(&self, task_id: &str) -> Result<PreparedRemoteTaskCheck> {
        self.prepare_remote_task_observation(task_id, None)
    }

    pub fn prepare_remote_task_observation(
        &self,
        task_id: &str,
        wait_ms: Option<u64>,
    ) -> Result<PreparedRemoteTaskCheck> {
        if wait_ms.is_some_and(|ms| !(1..=fliuno_protocol::MAX_REMOTE_TASK_WATCH_MS).contains(&ms))
        {
            return Err(Error::Invalid(
                "remote task waitMs must be 1 to 5000".into(),
            ));
        }
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
        if wait_ms.is_some()
            && (!backend.supports_remote_task_watch()
                || expected.state.is_terminal()
                || expected.context_id.is_none())
        {
            return Err(Error::Conflict(
                "remote task does not support watching".into(),
            ));
        }
        Ok(PreparedRemoteTaskCheck {
            task_id: task.id,
            run_id: run.id,
            check_id: new_id("remote_check"),
            expected,
            backend: Arc::clone(backend),
            wait: wait_ms.map(Duration::from_millis),
        })
    }

    pub fn record_remote_task_check(&mut self, check: CompletedRemoteTaskCheck) -> Result<()> {
        self.append_remote_task_check(&check.task_id, check.run_id, check.check_id, check.outcome)
    }
}
