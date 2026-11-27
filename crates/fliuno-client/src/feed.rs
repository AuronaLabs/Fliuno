//! A cursor-driven subscription that uses live notifications to locate durable pages.
use super::{Client, Error, EventReplay, Notification, ReplayScope};
use fliuno_protocol::{Event, TaskChange, TaskSummaryChangesPage};
use std::io::{BufRead, Write};

/// Bounded event delivery over a connection's current subscription scope.
/// Route notifications from other RPC calls through `observe` while it is active.
pub struct EventFeed {
    replay: EventReplay,
    signals: Signals,
}

#[derive(Default)]
struct Signals {
    through: u64,
    gap: bool,
}

impl Signals {
    fn observe(&mut self, notification: Notification) -> Option<Notification> {
        let sequence = match notification.method.as_str() {
            "notifications/event" => notification.params["event"]["sequence"].as_u64(),
            "notifications/eventWatermark" => notification.params["throughSequence"].as_u64(),
            "notifications/eventGap" => {
                self.gap = true;
                return None;
            }
            _ => return Some(notification),
        };
        match sequence {
            Some(sequence) if (1..=i64::MAX as u64).contains(&sequence) => {
                self.through = self.through.max(sequence);
            }
            _ => self.gap = true,
        }
        None
    }
}

impl EventFeed {
    /// Subscribe and prepare to read after the caller's last committed cursor.
    pub fn subscribe<R: BufRead, W: Write>(
        client: &mut Client<R, W>,
        scope: ReplayScope,
        after: u64,
        on_notification: impl FnMut(Notification),
    ) -> Result<Self, Error> {
        subscribe(client, &scope, after, on_notification).map(Self::from_replay)
    }

    pub(crate) fn from_replay(replay: EventReplay) -> Self {
        Self {
            replay,
            signals: Signals::default(),
        }
    }

    /// The current durable cursor. Commit it together with each consumed page.
    pub fn cursor(&self) -> u64 {
        self.replay.cursor()
    }

    pub(crate) fn through(&self) -> u64 {
        self.replay.through()
    }

    /// Whether a snapshot consumer has seen a gap or a newer durable watermark.
    pub fn snapshot_stale(&self, through: u64) -> bool {
        self.signals.gap || self.signals.through > through
    }

    /// Whether a snapshot consumer should replace the event subscription.
    pub fn subscription_lost(&self) -> bool {
        self.signals.gap
    }

    /// Record event or gap notifications and return other notifications unchanged.
    pub fn observe(&mut self, notification: Notification) -> Option<Notification> {
        self.signals.observe(notification)
    }

    /// Return a page of committed events or `None` after catching up to the known
    /// watermark. On `None`, keep reading notifications or making normal RPC calls.
    /// The feed coalesces live notifications into a watermark and retains no bodies.
    pub fn next_page<R: BufRead, W: Write>(
        &mut self,
        client: &mut Client<R, W>,
        limit: u32,
        target_bytes: Option<u32>,
        on_notification: impl FnMut(Notification),
    ) -> Result<Option<Vec<Event>>, Error> {
        self.next_with(client, on_notification, |replay, client, observe| {
            replay.next_page(client, limit, target_bytes, observe)
        })
    }

    /// Read changed task IDs through the known watermark without fetching event
    /// bodies. Shares the same cursor and gap recovery as `next_page`.
    pub fn next_task_changes<R: BufRead, W: Write>(
        &mut self,
        client: &mut Client<R, W>,
        limit: u32,
        on_notification: impl FnMut(Notification),
    ) -> Result<Option<Vec<TaskChange>>, Error> {
        self.next_with(client, on_notification, |replay, client, observe| {
            replay.next_task_changes(client, limit, observe)
        })
    }

    /// Refresh changed tasks with one call per page through the known event
    /// watermark. Shares the event cursor and gap recovery with `next_page`.
    pub fn next_task_summary_changes<R: BufRead, W: Write>(
        &mut self,
        client: &mut Client<R, W>,
        limit: u32,
        on_notification: impl FnMut(Notification),
    ) -> Result<Option<TaskSummaryChangesPage>, Error> {
        self.next_with(client, on_notification, |replay, client, observe| {
            replay.next_task_summary_changes(client, limit, observe)
        })
    }

    /// Reconcile actual summary revisions after the index was enabled, using
    /// the same durable cursor and gap recovery as `next_page`.
    pub fn next_task_summary_revisions<R: BufRead, W: Write>(
        &mut self,
        client: &mut Client<R, W>,
        limit: u32,
        on_notification: impl FnMut(Notification),
    ) -> Result<Option<TaskSummaryChangesPage>, Error> {
        self.next_with(client, on_notification, |replay, client, observe| {
            replay.next_task_summary_revisions(client, limit, observe)
        })
    }

    /// Reconcile one revision per task in each pinned event window.
    /// Check `latestTaskSummaryRevisionReplay` before selecting this reader.
    pub fn next_latest_task_summary_revisions<R: BufRead, W: Write>(
        &mut self,
        client: &mut Client<R, W>,
        limit: u32,
        on_notification: impl FnMut(Notification),
    ) -> Result<Option<TaskSummaryChangesPage>, Error> {
        self.next_with(client, on_notification, |replay, client, observe| {
            replay.next_latest_task_summary_revisions(client, limit, observe)
        })
    }

    fn next_with<R: BufRead, W: Write, T>(
        &mut self,
        client: &mut Client<R, W>,
        mut on_notification: impl FnMut(Notification),
        mut read: impl FnMut(
            &mut EventReplay,
            &mut Client<R, W>,
            &mut dyn FnMut(Notification),
        ) -> Result<Option<T>, Error>,
    ) -> Result<Option<T>, Error> {
        loop {
            if let Some(page) = read(&mut self.replay, client, &mut |notification| {
                if let Some(note) = self.signals.observe(notification) {
                    on_notification(note);
                }
            })? {
                return Ok(Some(page));
            }
            let after = self.replay.cursor();
            if self.signals.gap {
                self.replay = subscribe(client, self.replay.scope(), after, &mut on_notification)?;
                self.signals = Signals::default();
            } else if self.signals.through > after {
                self.replay =
                    EventReplay::new(self.replay.scope().clone(), after, self.signals.through)?;
            } else {
                return Ok(None);
            }
        }
    }
}

fn subscribe<R: BufRead, W: Write>(
    client: &mut Client<R, W>,
    scope: &ReplayScope,
    after: u64,
    mut on_notification: impl FnMut(Notification),
) -> Result<EventReplay, Error> {
    // These notifications belong to the prior subscription. Its durable events
    // are covered by the new replay window, and its gaps no longer apply.
    let watermark_only = client
        .info()
        .capabilities
        .get("eventWatermarkNotifications")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    client.subscribe_events_with_watermarks(scope.clone(), after, watermark_only, |note| {
        if !matches!(
            note.method.as_str(),
            "notifications/event" | "notifications/eventWatermark" | "notifications/eventGap"
        ) {
            on_notification(note);
        }
    })
}
