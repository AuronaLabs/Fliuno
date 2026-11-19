//! Checked paging of durable events through an inclusive server watermark.
use super::{Client, Error, Notification};
use fliuno_protocol::{
    Event, EventsListAllParams, EventsListParams, EventsListSessionParams,
    EventsListWorkspaceParams, EventsPage,
};
use std::io::{BufRead, Write};

#[derive(Debug, Clone)]
pub enum ReplayScope {
    All,
    Task(String),
    Session(String),
    Workspace(String),
}

/// A fixed replay window. The caller commits delivered events and its progress
/// together if it needs to recover after an application restart.
#[derive(Debug)]
pub struct EventReplay {
    scope: ReplayScope,
    through: u64,
    after: u64,
}

impl EventReplay {
    pub fn new(scope: ReplayScope, after: u64, through: u64) -> Result<Self, Error> {
        if after > through || through > i64::MAX as u64 {
            return Err(Error::InvalidReplayWindow);
        }
        Ok(Self {
            scope,
            through,
            after,
        })
    }

    /// Inclusive high-water mark of the delivered interval. Reaching the end
    /// advances to the captured watermark even when some sequences were out of scope.
    pub fn cursor(&self) -> u64 {
        self.after
    }

    /// Read one nonempty page, or return None when caught up. Notifications are passed to
    /// the caller, which can buffer them separately until replay is complete.
    pub fn next_page<R: BufRead, W: Write>(
        &mut self,
        client: &mut Client<R, W>,
        limit: u32,
        target_bytes: Option<u32>,
        on_notification: impl FnMut(Notification),
    ) -> Result<Option<Vec<Event>>, Error> {
        let after = self.after;
        let through = Some(self.through);
        if after == self.through {
            return Ok(None);
        }
        let page: EventsPage = match &self.scope {
            ReplayScope::All => client.call(
                "events/listAll",
                &EventsListAllParams {
                    after,
                    through,
                    limit,
                    target_bytes,
                },
                on_notification,
            ),
            ReplayScope::Task(id) => client.call(
                "events/list",
                &EventsListParams {
                    task_id: id.clone(),
                    after,
                    through,
                    limit,
                    target_bytes,
                },
                on_notification,
            ),
            ReplayScope::Session(id) => client.call(
                "events/listSession",
                &EventsListSessionParams {
                    session_id: id.clone(),
                    after,
                    through,
                    limit,
                    target_bytes,
                },
                on_notification,
            ),
            ReplayScope::Workspace(id) => client.call(
                "events/listWorkspace",
                &EventsListWorkspaceParams {
                    workspace_id: id.clone(),
                    after,
                    through,
                    limit,
                    target_bytes,
                },
                on_notification,
            ),
        }?;
        let mut last = after;
        for event in &page.events {
            if event.sequence <= last
                || event.sequence > self.through
                || matches!(&self.scope, ReplayScope::Task(task_id) if task_id != &event.task_id)
            {
                return Err(Error::InvalidReplayPage);
            }
            last = event.sequence;
        }
        if page.next_after != last
            || page.has_more && (last == after || last == self.through)
            || page.events.len() > limit as usize
        {
            return Err(Error::InvalidReplayPage);
        }
        self.after = if page.has_more { last } else { self.through };
        Ok((!page.events.is_empty()).then_some(page.events))
    }
}
