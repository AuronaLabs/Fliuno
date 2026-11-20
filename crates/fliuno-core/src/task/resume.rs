//! Read-only resume decisions and the corresponding durable state transition.
use super::*;
use crate::storage::ResumeFacts;
use TaskResumeDisposition as Disposition;
use std::borrow::Cow;

impl ResumeFacts<'_> {
    fn remaining_model_steps(&self) -> Option<u32> {
        (*self.backend == TaskBackend::Model)
            .then(|| remaining_model_steps(self.max_model_steps, self.model_steps))
    }
}

impl<'a> From<&'a Task> for ResumeFacts<'a> {
    fn from(task: &'a Task) -> Self {
        Self {
            status: task.status,
            model_steps: task.model_steps,
            max_model_steps: task.max_model_steps,
            backend: Cow::Borrowed(&task.backend),
            backend_authority: task.backend_authority.as_deref().map(Cow::Borrowed),
            agent_run: task.agent_run.is_some(),
            remote_continuation: task.remote_continuation.is_some(),
            in_flight_tool: task.in_flight_tool.is_some(),
            model_outcome_unknown: task.model_outcome_unknown,
        }
    }
}

impl Kernel {
    /// Inspect current recovery policy without queuing work or changing evidence.
    pub fn get_task_resume_status(&self, task_id: &str) -> Result<TaskResumeStatus> {
        let task = self.read_resume_facts(task_id)?;
        Ok(self.resume_status(task_id, &task))
    }

    /// Read all requested recovery decisions at one event watermark.
    pub fn get_task_resume_statuses(
        &self,
        request: &TaskSummaryBatchParams,
    ) -> Result<TaskResumeStatusBatch> {
        let (items, through_sequence) = self.read_resume_facts_batch(request)?;
        Ok(TaskResumeStatusBatch {
            items: items
                .into_iter()
                .zip(&request.task_ids)
                .map(|(task, id)| task.map(|facts| self.resume_status(id, &facts)))
                .collect(),
            through_sequence,
        })
    }

    fn resume_status(&self, id: &str, task: &ResumeFacts<'_>) -> TaskResumeStatus {
        TaskResumeStatus {
            disposition: self.resume_disposition(task),
            remaining_model_steps: task.remaining_model_steps(),
            status: task.status,
            task_id: id.into(),
        }
    }

    fn resume_disposition(&self, task: &ResumeFacts<'_>) -> Disposition {
        if task.status != TaskStatus::Interrupted {
            return Disposition::NotInterrupted;
        }
        if task.remote_continuation {
            return Disposition::RemoteContinuation;
        }
        if task.agent_run {
            return Disposition::AgentAlreadyStarted;
        }
        if let TaskBackend::ExternalAgent { agent_id } = task.backend.as_ref() {
            let Some(backend) = self.agents.get(agent_id) else {
                return Disposition::AgentUnavailable;
            };
            if task.backend_authority.as_deref() != Some(backend.authority().as_str()) {
                return Disposition::AgentRegistrationChanged;
            }
        }
        if task.in_flight_tool {
            return Disposition::ToolOutcomeUnknown;
        }
        if task.remaining_model_steps() == Some(0) {
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
        let reason = match self.resume_disposition(&ResumeFacts::from(&task)) {
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
