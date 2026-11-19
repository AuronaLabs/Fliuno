//! Walk a durable view of one pending approval inbox.
use crate::{Client, Error, Notification};
use fliuno_protocol::{ApprovalInboxListParams, ApprovalInboxPage};
use std::{
    collections::HashSet,
    io::{BufRead, Write},
};

#[derive(Debug)]
pub struct ApprovalInbox {
    request: Option<ApprovalInboxListParams>,
}

impl ApprovalInbox {
    pub fn new(request: ApprovalInboxListParams) -> Self {
        Self {
            request: Some(request),
        }
    }

    /// Persist this request together with the page it follows.
    pub fn next_request(&self) -> Option<&ApprovalInboxListParams> {
        self.request.as_ref()
    }

    /// Fetch one page, including an empty initial inbox. A rejected page or
    /// failed RPC leaves the checkpoint unchanged.
    pub fn next_page<R: BufRead, W: Write>(
        &mut self,
        client: &mut Client<R, W>,
        on_notification: impl FnMut(Notification),
    ) -> Result<Option<ApprovalInboxPage>, Error> {
        let Some(request) = &mut self.request else {
            return Ok(None);
        };
        let reply: ApprovalInboxPage =
            client.call("approval/listPendingSnapshot", request, on_notification)?;
        let page = &reply.page;
        if request
            .snapshot
            .is_some_and(|stamp| stamp != reply.snapshot)
            || reply.snapshot.through_sequence > i64::MAX as u64
            || reply.snapshot.captured_at_unix_ms > i64::MAX as u64
            || page.items.len() > request.page.limit as usize
            || page.next_after > i64::MAX as u64
            || (page.items.is_empty() && (page.has_more || page.next_after != request.page.after))
            || (!page.items.is_empty() && page.next_after <= request.page.after)
        {
            return Err(Error::InvalidApprovalInboxPage);
        }
        let mut ids = HashSet::with_capacity(page.items.len());
        if page.items.iter().any(|item| {
            item.id.is_empty()
                || item.task_id.is_empty()
                || item.session_id.is_empty()
                || request
                    .page
                    .session_id
                    .as_ref()
                    .is_some_and(|id| id != &item.session_id)
                || item
                    .expires_at_unix_ms
                    .is_some_and(|time| time <= reply.snapshot.captured_at_unix_ms)
                || !ids.insert(&item.id)
        }) {
            return Err(Error::InvalidApprovalInboxPage);
        }
        if page.has_more {
            request.snapshot = Some(reply.snapshot);
            request.page.after = page.next_after;
        } else {
            self.request = None;
        }
        Ok(Some(reply))
    }
}
