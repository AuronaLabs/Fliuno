//! Read the kernel's explicit recovery decision for one task.
use super::{Client, Error, Notification};
use fliuno_protocol::{TaskIdParams, TaskResumeDisposition, TaskResumeStatus, TaskStatus};
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
        if reply.task_id != task_id
            || (reply.status == TaskStatus::Interrupted)
                == (reply.disposition == TaskResumeDisposition::NotInterrupted)
        {
            return Err(Error::InvalidResult);
        }
        Ok(reply)
    }
}
