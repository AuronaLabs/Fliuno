//! Read-only resume decisions and the corresponding durable state transition.
use super::*;
use TaskResumeDisposition as Disposition;

impl Kernel {
    /// Inspect current recovery policy without queuing work or changing evidence.
    pub fn get_task_resume_status(&self, task_id: &str) -> Result<TaskResumeStatus> {
        let task = self.get_task(task_id)?;
        Ok(TaskResumeStatus {
            disposition: self.resume_disposition(&task),
            remaining_model_steps: (task.backend == TaskBackend::Model)
                .then(|| remaining_model_steps(&task)),
            status: task.status,
            task_id: task.id,
        })
    }

    fn resume_disposition(&self, task: &Task) -> Disposition {
        if task.status != TaskStatus::Interrupted {
            return Disposition::NotInterrupted;
        }
        if task.remote_continuation.is_some() {
            return Disposition::RemoteContinuation;
        }
        if task.agent_run.is_some() {
            return Disposition::AgentAlreadyStarted;
        }
        if let TaskBackend::ExternalAgent { agent_id } = &task.backend {
            let Some(backend) = self.agents.get(agent_id) else {
                return Disposition::AgentUnavailable;
            };
            if task.backend_authority.as_deref() != Some(backend.authority().as_str()) {
                return Disposition::AgentRegistrationChanged;
            }
        }
        if task.in_flight_tool.is_some() {
            return Disposition::ToolOutcomeUnknown;
        }
        if task.backend == TaskBackend::Model && model_step_budget_spent(task) {
            return Disposition::ModelStepBudgetExhausted;
        }
        if task.model_outcome_unknown {
            return Disposition::RetryUncertainModelStep;
        }
        Disposition::Ready
    }

    pub fn resume_task(&mut self, task_id: &str) -> Result<Task> {
        self.resume_task_with_options(task_id, false)
    }

    pub fn resume_task_with_options(
        &mut self,
        task_id: &str,
        retry_uncertain_model_step: bool,
    ) -> Result<Task> {
        let task = self.get_task(task_id)?;
        if task.backend != TaskBackend::Model {
            return Err(Error::Conflict(
                "external agent tasks require the background execution scheduler".into(),
            ));
        }
        let task = self.transition_to_resumed(task, retry_uncertain_model_step)?;
        self.drive(task)
    }

    pub fn prepare_resume_task(
        &mut self,
        task_id: &str,
        retry_uncertain_model_step: bool,
    ) -> Result<Task> {
        let task = self.get_task(task_id)?;
        self.transition_to_resumed(task, retry_uncertain_model_step)
    }

    fn transition_to_resumed(&mut self, mut task: Task, consent: bool) -> Result<Task> {
        let reason = match self.resume_disposition(&task) {
            Disposition::Ready => None,
            Disposition::RetryUncertainModelStep if consent => None,
            Disposition::RetryUncertainModelStep => {
                Some("model request outcome is uncertain; explicit retry consent is required")
            }
            Disposition::NotInterrupted => Some("only interrupted tasks can resume"),
            Disposition::RemoteContinuation => Some(
                "remote continuations cannot resume; inspect submission evidence before creating another attempt",
            ),
            Disposition::AgentAlreadyStarted => Some(
                "started agent runs cannot be replayed; create a new task after inspecting the outcome",
            ),
            Disposition::AgentUnavailable => {
                return Err(Error::NotFound("registered agent".into()));
            }
            Disposition::AgentRegistrationChanged => {
                Some("agent registration changed or lacks an authority binding; create a new task")
            }
            Disposition::ToolOutcomeUnknown => Some(
                "external tool outcome unknown; inspect the remote service and cancel this task",
            ),
            Disposition::ModelStepBudgetExhausted => {
                Some("model step budget exhausted; task cannot resume")
            }
        };
        if let Some(reason) = reason {
            return Err(Error::Conflict(reason.into()));
        }
        task.model_outcome_unknown = false;
        task.status = TaskStatus::Running;
        task.output = None;
        self.commit(&task, &[EventKind::TaskResumed], &[])?;
        Ok(task)
    }
}
