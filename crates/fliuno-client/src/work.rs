//! Bootstrap a work view from a snapshot and follow its durable events.
use super::{ApprovalInbox, Client, Error, EventFeed, EventReplay, Notification, ReplayScope};
use fliuno_protocol::{WorkSnapshot, WorkSnapshotParams};
use std::io::{BufRead, Write};

/// Overview, approval continuation, and live feed sharing one snapshot cursor.
pub struct WorkSubscription {
    pub snapshot: WorkSnapshot,
    pub inbox: ApprovalInbox,
    pub events: EventFeed,
}

impl<R: BufRead, W: Write> Client<R, W> {
    /// Read a work overview and an inbox pager already positioned after its
    /// first page. Use the snapshot's global event sequence to resume delivery.
    pub fn work_snapshot(
        &mut self,
        params: &WorkSnapshotParams,
        on_notification: impl FnMut(Notification),
    ) -> Result<(WorkSnapshot, ApprovalInbox), Error> {
        let snapshot: WorkSnapshot = self.call("work/getSnapshot", params, on_notification)?;
        let inbox = ApprovalInbox::after_first_page(params, &snapshot.approvals)?;
        Ok((snapshot, inbox))
    }

    /// Read the work snapshot and replace the event subscription in one request.
    /// Any durable notifications delivered before the response belong to the
    /// previous subscription. Route subsequent notices to the returned feed.
    pub fn subscribe_work(
        &mut self,
        params: &WorkSnapshotParams,
        on_notification: impl FnMut(Notification),
    ) -> Result<WorkSubscription, Error> {
        let snapshot: WorkSnapshot = self.call("work/subscribe", params, on_notification)?;
        let inbox = ApprovalInbox::after_first_page(params, &snapshot.approvals)?;
        let scope = match (&params.session_id, &params.workspace_id) {
            (Some(id), None) => ReplayScope::Session(id.clone()),
            (None, Some(id)) => ReplayScope::Workspace(id.clone()),
            (None, None) => ReplayScope::All,
            (Some(_), Some(_)) => return Err(Error::InvalidResult),
        };
        let through = snapshot.approvals.snapshot.through_sequence;
        let events = EventFeed::from_replay(EventReplay::new(scope, through, through)?);
        Ok(WorkSubscription {
            snapshot,
            inbox,
            events,
        })
    }
}
