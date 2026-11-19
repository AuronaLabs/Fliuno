//! Validate ordered receipts for atomically cancelling selected tasks.
use super::{Client, Error, Notification};
use fliuno_protocol::{TaskCancelBatch, TaskCancelBatchParams, TaskCancelScope, TaskStatus};
use std::io::{BufRead, Write};

impl<R: BufRead, W: Write> Client<R, W> {
    /// Commit cancellation in one RPC. Retrying does not repeat an event, and
    /// the receipt does not wait for in-flight provider or tool cleanup.
    pub fn cancel_task_batch(
        &mut self,
        request: &TaskCancelBatchParams,
        on_notification: impl FnMut(Notification),
    ) -> Result<TaskCancelBatch, Error> {
        let batch: TaskCancelBatch = self.call("task/cancelBatch", request, on_notification)?;
        if batch.items.len() != request.task_ids.len()
            || batch.through_sequence > i64::MAX as u64
            || batch.items.iter().zip(&request.task_ids).any(|(item, id)| {
                &item.task.id != id
                    || !item.task.status.is_terminal()
                    || (item.changed && item.task.status != TaskStatus::Cancelled)
                    || matches!(&request.scope, TaskCancelScope::Session { session_id } if &item.task.session_id != session_id)
            })
        {
            return Err(Error::InvalidResult);
        }
        Ok(batch)
    }
}
