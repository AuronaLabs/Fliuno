//! Read the kernel's explicit recovery decisions for interrupted tasks.
use super::{Client, Error, Notification};
use fliuno_protocol::{
    TaskIdParams, TaskResumeDisposition, TaskResumeStatus, TaskResumeStatusBatch, TaskStatus,
    TaskSummaryBatchParams,
};
use std::io::{BufRead, Write};

impl<R: BufRead, W: Write> Client<R, W> {
    /// Read without resuming; hosts can detect support through `taskResumeStatus`.
    pub fn get_task_resume_status(
        &mut self,
        task_id: &str,
        on_notification: impl FnMut(Notification),
    ) -> Result<TaskResumeStatus, Error> {
        let reply: TaskResumeStatus = self.call(
            "task/getResumeStatus",
            &TaskIdParams {
                task_id: task_id.into(),
            },
            on_notification,
        )?;
        if !valid_status(task_id, &reply) {
            return Err(Error::InvalidResult);
        }
        Ok(reply)
    }

    /// Read recovery decisions in request order when `taskResumeStatusBatch` is available.
    pub fn get_task_resume_statuses(
        &mut self,
        request: &TaskSummaryBatchParams,
        on_notification: impl FnMut(Notification),
    ) -> Result<TaskResumeStatusBatch, Error> {
        let reply: TaskResumeStatusBatch =
            self.call("task/getResumeStatuses", request, on_notification)?;
        if reply.items.len() != request.task_ids.len()
            || reply.through_sequence > i64::MAX as u64
            || reply.items.iter().zip(&request.task_ids).any(|(item, id)| {
                item.as_ref()
                    .is_some_and(|status| !valid_status(id, status))
            })
        {
            return Err(Error::InvalidResult);
        }
        Ok(reply)
    }
}

fn valid_status(id: &str, status: &TaskResumeStatus) -> bool {
    status.task_id == id
        && (status.status == TaskStatus::Interrupted)
            != (status.disposition == TaskResumeDisposition::NotInterrupted)
}
