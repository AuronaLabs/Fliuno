//! Checked paging of durable events through an inclusive server watermark.
mod task_changes;

use super::{Client, Error, Notification};
use fliuno_protocol::{
    Event, EventKind, EventSubscription, EventWatermark, EventsListAllParams, EventsListParams,
    EventsListSessionParams, EventsListWorkspaceParams, EventsPage, EventsSetScopeParams,
};
use std::io::{BufRead, Write};

#[derive(Debug, Clone)]
pub enum ReplayScope {
    All,
    Task(String),
    Session(String),
    Workspace(String),
}

/// Scopes that support indexed replay of reported model usage.
#[derive(Debug, Clone)]
pub enum ModelUsageScope {
    Task(String),
    Session(String),
    Workspace(String),
}

impl<R: BufRead, W: Write> Client<R, W> {
    /// Read the committed event watermark without changing the live subscription.
    pub fn event_watermark(
        &mut self,
        on_notification: impl FnMut(Notification),
    ) -> Result<u64, Error> {
        let watermark: EventWatermark = self.call(
            "events/getWatermark",
            &serde_json::json!({}),
            on_notification,
        )?;
        if watermark.through_sequence > i64::MAX as u64 {
            return Err(Error::InvalidReplayWindow);
        }
        Ok(watermark.through_sequence)
    }

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
        self.subscribe_events_with_watermarks(scope, after, false, on_notification)
    }

    pub(crate) fn subscribe_events_with_watermarks(
        &mut self,
        scope: ReplayScope,
        after: u64,
        watermark_only: bool,
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
            watermark_only: watermark_only.then_some(true),
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
    model_usage_only: bool,
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
            model_usage_only: false,
            through,
            after,
        })
    }

    /// Replay only reported usage without reading the other events in the scope.
    /// For a fresh window, obtain `through` with `Client::event_watermark`.
    pub fn for_model_usage(
        scope: ModelUsageScope,
        after: u64,
        through: u64,
    ) -> Result<Self, Error> {
        let scope = match scope {
            ModelUsageScope::Task(id) => ReplayScope::Task(id),
            ModelUsageScope::Session(id) => ReplayScope::Session(id),
            ModelUsageScope::Workspace(id) => ReplayScope::Workspace(id),
        };
        let mut replay = Self::new(scope, after, through)?;
        replay.model_usage_only = true;
        Ok(replay)
    }

    pub(crate) fn scope(&self) -> &ReplayScope {
        &self.scope
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
                if self.model_usage_only {
                    "events/listModelUsage"
                } else {
                    "events/list"
                },
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
                if self.model_usage_only {
                    "events/listSessionModelUsage"
                } else {
                    "events/listSession"
                },
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
                if self.model_usage_only {
                    "events/listWorkspaceModelUsage"
                } else {
                    "events/listWorkspace"
                },
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
                || (self.model_usage_only
                    && !matches!(event.data, EventKind::ModelUsageReported { .. }))
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
