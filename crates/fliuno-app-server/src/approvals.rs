//! Approval RPCs share one preparation and scheduling path.
use super::{RpcError, RpcServer};
use fliuno_core::{ApprovalOutcome, Error, ToolInputOutcome};
use fliuno_protocol::{
    ApprovalRespondParams, ApprovalResumeParams, ApprovalRun, Event, EventKind, Task,
    TaskRequestLookupParams, TaskSummary, ToolInputRespondParams,
};

impl RpcServer {
    pub(super) fn respond_tool_input(
        &self,
        params: ToolInputRespondParams,
    ) -> Result<Task, RpcError> {
        let outcome = self.kernel()?.prepare_tool_input_response(&params)?;
        let task = match outcome {
            ToolInputOutcome::Duplicate(task) => task,
            ToolInputOutcome::Prepared { task, execution } => {
                match self.scheduler.schedule_tool(execution) {
                    Ok(()) => task,
                    Err(message) => self.kernel()?.fail_task(&task.id, message.into())?,
                }
            }
        };
        let followups = self.kernel()?.advance_runs_for_task(&task.id)?;
        self.schedule_run_tasks(followups)?;
        Ok(task)
    }

    pub(super) fn cancel_approval_run(
        &self,
        p: TaskRequestLookupParams,
    ) -> Result<TaskSummary, RpcError> {
        let mut kernel = self.kernel()?;
        let task = kernel.cancel_approval_run(&p.session_id, &p.client_request_id)?;
        self.scheduler
            .cancel(&task.id)
            .map_err(|message| RpcError {
                code: -32603,
                message: message.into(),
            })?;
        Ok(task)
    }

    pub(super) fn resume_approval_run(
        &self,
        p: ApprovalResumeParams,
    ) -> Result<ApprovalRun, RpcError> {
        let outcome = self.kernel()?.resume_approval_run(&p)?;
        if let Some(outcome) = outcome {
            self.schedule_approval(outcome)?;
        }
        Ok(self
            .kernel()?
            .get_approval_run(&p.session_id, &p.client_request_id)?)
    }

    pub(super) fn respond_approval(&self, params: ApprovalRespondParams) -> Result<Task, RpcError> {
        let outcome = self.kernel()?.prepare_approval_response(
            &params.task_id,
            &params.approval_id,
            params.decision,
        )?;
        self.schedule_approval(outcome)
    }

    pub(super) fn respond_approval_decision(
        &self,
        params: ApprovalRespondParams,
    ) -> Result<Event, RpcError> {
        let outcome = {
            let mut kernel = self.kernel()?;
            match kernel.get_approval_decision(&params.task_id, &params.approval_id) {
                Ok(event) => {
                    let committed = match event.data {
                        EventKind::ApprovalResolved { decision, .. }
                        | EventKind::AgentApprovalResolved { decision, .. } => decision,
                        _ => {
                            return Err(RpcError {
                                code: -32603,
                                message: "invalid stored approval decision".into(),
                            });
                        }
                    };
                    if committed != params.decision {
                        return Err(Error::Conflict(
                            "a different approval decision is already recorded".into(),
                        )
                        .into());
                    }
                    return Ok(event);
                }
                Err(Error::NotFound(_)) => {}
                Err(error) => return Err(error.into()),
            }
            kernel.get_pending_approval(&params.task_id, &params.approval_id)?;
            kernel.prepare_approval_response(
                &params.task_id,
                &params.approval_id,
                params.decision,
            )?
        };
        self.schedule_approval(outcome)?;
        Ok(self
            .kernel()?
            .get_approval_decision(&params.task_id, &params.approval_id)?)
    }

    fn schedule_approval(&self, outcome: ApprovalOutcome) -> Result<Task, RpcError> {
        let task = match outcome {
            ApprovalOutcome::Finished(task) => task,
            ApprovalOutcome::Prepared { task, execution } => {
                match self.scheduler.schedule_tool(execution) {
                    Ok(()) => task,
                    Err(message) => self.kernel()?.fail_task(&task.id, message.into())?,
                }
            }
        };
        let followups = self.kernel()?.advance_runs_for_task(&task.id)?;
        self.schedule_run_tasks(followups)?;
        Ok(task)
    }
}
