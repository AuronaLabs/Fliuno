//! Submit bounded user input to resume an approved tool call.
use super::{Client, Error, Notification};
use fliuno_protocol::{Task, ToolInputRespondParams};
use std::io::{BufRead, Write};

impl<R: BufRead, W: Write> Client<R, W> {
    /// Resume the exact tool call identified by `call_id` after reviewing its request.
    pub fn respond_tool_input(
        &mut self,
        request: &ToolInputRespondParams,
        on_notification: impl FnMut(Notification),
    ) -> Result<Task, Error> {
        let task: Task = self.call("task/respondToolInput", request, on_notification)?;
        if task.id != request.task_id
            || (task.status == fliuno_protocol::TaskStatus::WaitingInput
                && task
                    .pending_tool_input
                    .as_ref()
                    .is_none_or(|pending| pending.call_id != request.call_id))
        {
            return Err(Error::InvalidResult);
        }
        Ok(task)
    }
}
