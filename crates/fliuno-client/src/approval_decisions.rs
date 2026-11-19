//! Reconcile committed approval decisions in one read-only request.
use super::{Client, Error, Notification};
use fliuno_protocol::{ApprovalDecisionBatch, ApprovalDecisionBatchParams};
use std::{
    collections::HashMap,
    io::{BufRead, Write},
};

impl<R: BufRead, W: Write> Client<R, W> {
    /// Read small receipts in input order. A missing receipt is not consent
    /// and does not say whether the approval was pending, closed, or unknown.
    /// Check the server's `approvalDecisionBatch` capability before calling.
    pub fn get_approval_decisions(
        &mut self,
        request: &ApprovalDecisionBatchParams,
        on_notification: impl FnMut(Notification),
    ) -> Result<ApprovalDecisionBatch, Error> {
        let batch: ApprovalDecisionBatch =
            self.call("approval/getDecisions", request, on_notification)?;
        if batch.items.len() != request.approvals.len() || batch.through_sequence > i64::MAX as u64
        {
            return Err(Error::InvalidResult);
        }
        let mut by_key = HashMap::with_capacity(request.approvals.len());
        for (key, item) in request.approvals.iter().zip(&batch.items) {
            if item.as_ref().is_some_and(|receipt| {
                receipt.task_id != key.task_id
                    || receipt.approval_id != key.approval_id
                    || !(1..=batch.through_sequence).contains(&receipt.sequence)
                    || receipt.run_id.as_ref().is_some_and(String::is_empty)
            }) || by_key.insert(key, item).is_some_and(|prior| prior != item)
            {
                return Err(Error::InvalidResult);
            }
        }
        Ok(batch)
    }
}
