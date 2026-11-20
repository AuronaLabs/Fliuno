//! Fetch a bounded set of summaries without losing input alignment.
use super::{Client, Error, Notification};
use fliuno_protocol::{TaskRequestSummaryBatchParams, TaskSummaryBatch, TaskSummaryBatchParams};
use std::io::{BufRead, Write};

impl<R: BufRead, W: Write> Client<R, W> {
    /// Read current summaries in request order at the returned event watermark.
    /// Missing and out-of-scope tasks preserve their positions as `None`.
    pub fn get_task_summaries(
        &mut self,
        request: &TaskSummaryBatchParams,
        on_notification: impl FnMut(Notification),
    ) -> Result<TaskSummaryBatch, Error> {
        let batch: TaskSummaryBatch = self.call("task/getSummaries", request, on_notification)?;
        if batch.items.len() != request.task_ids.len()
            || batch.through_sequence > i64::MAX as u64
            || batch.items.iter().zip(&request.task_ids).any(|(item, id)| {
                item.as_ref().is_some_and(|summary| {
                    &summary.id != id
                        || request
                            .session_id
                            .as_ref()
                            .is_some_and(|session| session != &summary.session_id)
                })
            })
        {
            return Err(Error::InvalidResult);
        }
        Ok(batch)
    }

    /// Resolve persisted task request keys at one watermark when the server
    /// advertises `taskSummaryByRequestBatch`. Keep the input order, including
    /// keys without an existing task binding.
    pub fn get_task_summaries_by_request(
        &mut self,
        request: &TaskRequestSummaryBatchParams,
        on_notification: impl FnMut(Notification),
    ) -> Result<TaskSummaryBatch, Error> {
        let batch: TaskSummaryBatch =
            self.call("task/getSummariesByRequest", request, on_notification)?;
        if batch.items.len() != request.client_request_ids.len()
            || batch.through_sequence > i64::MAX as u64
            || batch
                .items
                .iter()
                .flatten()
                .any(|summary| summary.session_id != request.session_id)
        {
            return Err(Error::InvalidResult);
        }
        Ok(batch)
    }
}
