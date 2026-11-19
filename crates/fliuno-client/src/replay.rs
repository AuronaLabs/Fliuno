//! Checked paging of durable events through an inclusive server watermark.
use super::{Client, Error, Notification};
use fliuno_protocol::{
    Event, EventSubscription, EventsListAllParams, EventsListParams, EventsListSessionParams,
    EventsListWorkspaceParams, EventsPage, EventsSetScopeParams,
};
use std::io::{BufRead, Write};

#[derive(Debug, Clone)]
pub enum ReplayScope {
    All,
    Task(String),
    Session(String),
    Workspace(String),
}

impl<R: BufRead, W: Write> Client<R, W> {
    /// Install a live subscription and open the matching replay window. `after`
    /// is the last consumed sequence for this scope, or zero for full replay.
    /// Calling again with the same scope also covers recovery from a queue gap.
    ///
    /// Durable notifications delivered to this call's callback still belong to
    /// the previous subscription. During subsequent calls, buffer new live
    /// events until replay reaches `EventReplay::through()`. Servers that advertise
    /// `eventSubscriptionCursor` reject a cursor ahead of the watermark before
    /// switching scope. On older servers, the replay window is checked after switching.
    pub fn subscribe_events(
        &mut self,
        scope: ReplayScope,
        after: u64,
        on_notification: impl FnMut(Notification),
    ) -> Result<EventReplay, Error> {
        if after > i64::MAX as u64 {
            return Err(Error::InvalidReplayWindow);
        }
        let mut params = EventsSetScopeParams {
            after: self
                .info()
                .capabilities
                .get("eventSubscriptionCursor")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false)
                .then_some(after),
            task_id: None,
            session_id: None,
            workspace_id: None,
        };
        match &scope {
            ReplayScope::All => {}
            ReplayScope::Task(id) => params.task_id = Some(id.clone()),
            ReplayScope::Session(id) => params.session_id = Some(id.clone()),
            ReplayScope::Workspace(id) => params.workspace_id = Some(id.clone()),
        }
        let reply: EventSubscription = self.call("events/setScope", &params, on_notification)?;
        EventReplay::new(scope, after, reply.through_sequence)
    }
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

    /// Inclusive end of this replay window. Live events start after this value.
    pub fn through(&self) -> u64 {
        self.through
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
