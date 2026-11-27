//! Bootstrap a work view from a snapshot and follow its durable events.
use super::{ApprovalInbox, Client, Error, EventFeed, EventReplay, Notification, ReplayScope};
use fliuno_protocol::{
    MAX_WORK_RECENT_TASKS, WorkSessionCounts, WorkSessionCountsParams, WorkSnapshot,
    WorkSnapshotParams,
};
use std::{
    collections::{HashMap, HashSet},
    io::{BufRead, Write},
};

#[cfg(test)]
mod tests;

/// Overview, approval continuation, and live feed sharing one snapshot cursor.
pub struct WorkSubscription {
    pub snapshot: WorkSnapshot,
    pub inbox: ApprovalInbox,
    pub events: EventFeed,
}

impl<R: BufRead, W: Write> Client<R, W> {
    /// Read task counts for visible sessions at one event watermark. Check
    /// the `workSessionCounts` capability before calling.
    pub fn work_session_counts(
        &mut self,
        request: &WorkSessionCountsParams,
        on_notification: impl FnMut(Notification),
    ) -> Result<WorkSessionCounts, Error> {
        let result: WorkSessionCounts =
            self.call("work/getSessionCounts", request, on_notification)?;
        if result.items.len() != request.session_ids.len()
            || result.through_sequence > i64::MAX as u64
        {
            return Err(Error::InvalidResult);
        }
        let mut seen = HashMap::with_capacity(result.items.len());
        for (id, item) in request.session_ids.iter().zip(&result.items) {
            if item.as_ref().is_some_and(|item| &item.session_id != id)
                || seen.insert(id, item).is_some_and(|prior| prior != item)
            {
                return Err(Error::InvalidResult);
            }
        }
        Ok(result)
    }

    /// Read a work overview and an inbox pager already positioned after its
    /// first page. Use the snapshot's global event sequence to resume delivery.
    /// Servers without `workRecentTasks` return no task preview.
    pub fn work_snapshot(
        &mut self,
        params: &WorkSnapshotParams,
        on_notification: impl FnMut(Notification),
    ) -> Result<(WorkSnapshot, ApprovalInbox), Error> {
        let snapshot = self.read_work_snapshot("work/getSnapshot", params, on_notification)?;
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
        let snapshot = self.read_work_snapshot("work/subscribe", params, on_notification)?;
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

    fn read_work_snapshot(
        &mut self,
        method: &str,
        params: &WorkSnapshotParams,
        on_notification: impl FnMut(Notification),
    ) -> Result<WorkSnapshot, Error> {
        let mut legacy;
        let params = if let Some(limit) = params.recent_task_limit {
            if !(1..=MAX_WORK_RECENT_TASKS).contains(&limit) {
                return Err(Error::InvalidRecentTaskLimit);
            }
            if self
                .info()
                .capabilities
                .get("workRecentTasks")
                .and_then(|value| value.as_bool())
                == Some(true)
            {
                params
            } else {
                legacy = params.clone();
                legacy.recent_task_limit = None;
                &legacy
            }
        } else {
            params
        };
        let snapshot: WorkSnapshot = self.call(method, params, on_notification)?;
        match (params.recent_task_limit, &snapshot.recent_tasks) {
            (None, None) => (),
            (Some(limit), Some(tasks)) => {
                let counts = &snapshot.task_counts;
                let expected = [
                    counts.running,
                    counts.waiting_approval,
                    counts.waiting_input,
                    counts.interrupted,
                    counts.completed,
                    counts.cancelled,
                    counts.failed,
                ]
                .into_iter()
                .fold(0u64, u64::saturating_add)
                .min(u64::from(limit));
                let mut ids = HashSet::with_capacity(tasks.len().min(limit as usize));
                if tasks.len() as u64 != expected
                    || tasks.iter().any(|task| {
                        params
                            .session_id
                            .as_ref()
                            .is_some_and(|id| task.session_id != *id)
                            || task.id.is_empty()
                            || task.session_id.is_empty()
                            || !ids.insert(task.id.as_str())
                    })
                {
                    return Err(Error::InvalidWorkPreview);
                }
            }
            _ => return Err(Error::InvalidWorkPreview),
        }
        Ok(snapshot)
    }
}
