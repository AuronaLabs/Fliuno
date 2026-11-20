//! Fetch the full details of several pending approvals for review.
use crate::{Client, Error, Notification};
use fliuno_protocol::{
    AgentApprovalSource, ApprovalDetails, ApprovalGetBatch, ApprovalGetBatchParams,
};
use std::{
    collections::HashMap,
    io::{BufRead, Write},
};

impl<R: BufRead, W: Write> Client<R, W> {
    /// Fetch live details in request order. Check `pendingApprovalDetailsBatch`
    /// first. Missing or expired approvals occupy their original positions.
    pub fn get_pending_approval_details(
        &mut self,
        request: &ApprovalGetBatchParams,
        on_notification: impl FnMut(Notification),
    ) -> Result<ApprovalGetBatch, Error> {
        let batch: ApprovalGetBatch = self.call("approval/getBatch", request, on_notification)?;
        if batch.items.len() != request.approvals.len() {
            return Err(Error::InvalidResult);
        }
        let mut seen = HashMap::with_capacity(batch.items.len());
        for (key, item) in request.approvals.iter().zip(&batch.items) {
            let matches_key = match item {
                None => true,
                Some(ApprovalDetails::ToolCall { approval }) => {
                    approval.id == key.approval_id && approval.task_id == key.task_id
                }
                Some(ApprovalDetails::AgentPermission { approval }) => {
                    approval.id == key.approval_id
                        && approval.source == AgentApprovalSource::AgentPermission
                }
                Some(ApprovalDetails::HostTool { approval }) => {
                    approval.id == key.approval_id
                        && approval.source == AgentApprovalSource::HostTool
                }
            };
            if !matches_key || seen.insert(key, item).is_some_and(|prior| prior != item) {
                return Err(Error::InvalidResult);
            }
        }
        Ok(batch)
    }
}
