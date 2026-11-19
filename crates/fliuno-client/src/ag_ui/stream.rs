//! Page-by-page SSE delivery over a task-scoped durable subscription.
use super::{AgUiRunPhase, AgUiSseError, AgUiSseStream};
use crate::{Client, Error, EventFeed, Notification, ReplayScope};
use fliuno_protocol::TaskSummary;
use std::io::{BufRead, Write};

pub(super) const PAGE_EVENTS: u32 = 64;
pub(super) const PAGE_BYTES: u32 = 64 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum AgUiSseFeedError {
    #[error(transparent)]
    Rpc(#[from] Error),
    #[error(transparent)]
    Sse(#[from] AgUiSseError),
}

/// A task-scoped SSE writer that replays committed events and recovers gaps.
/// One feed owns the connection's event subscription. Forward notifications
/// from the host's other calls on that connection through [`Self::observe`].
pub struct AgUiSseFeed {
    feed: EventFeed,
    stream: AgUiSseStream,
    failed: bool,
}

impl AgUiSseFeed {
    /// Subscribe to an existing kernel task using its derived run identity.
    /// For tasks started via AG-UI, use [`super::AgUiRun::subscribe_sse`].
    pub fn subscribe<R: BufRead, W: Write>(
        client: &mut Client<R, W>,
        task: &TaskSummary,
        last_event_id: Option<&str>,
        on_notification: impl FnMut(Notification),
    ) -> Result<Self, AgUiSseFeedError> {
        Self::with_stream(
            client,
            &task.id,
            AgUiSseStream::new(task, last_event_id)?,
            on_notification,
        )
    }

    pub(super) fn with_stream<R: BufRead, W: Write>(
        client: &mut Client<R, W>,
        task_id: &str,
        stream: AgUiSseStream,
        on_notification: impl FnMut(Notification),
    ) -> Result<Self, AgUiSseFeedError> {
        let feed = EventFeed::subscribe(
            client,
            ReplayScope::Task(task_id.into()),
            stream.after(),
            on_notification,
        )?;
        // A cursor cannot refer to a notification that commits after subscription.
        stream.check_watermark(feed.through())?;
        Ok(Self {
            feed,
            stream,
            failed: false,
        })
    }

    /// Return the latest projected phase, or `None` after an output or replay error.
    /// The phase changes only as pages are processed, including on reconnect.
    pub fn run_phase(&self) -> Option<AgUiRunPhase> {
        (!self.failed).then(|| self.stream.run_phase())
    }

    /// Record a durable notification and return any other notification unchanged.
    pub fn observe(&mut self, notification: Notification) -> Option<Notification> {
        self.feed.observe(notification)
    }

    /// Write one page and return its frame count, which can be zero when every
    /// event is skipped. `None` means caught up through the known watermark.
    /// Use a fresh feed and the last received SSE ID after an error, since the
    /// remote consumer may have received only part of the current page.
    pub fn write_next_page<R: BufRead, W: Write>(
        &mut self,
        client: &mut Client<R, W>,
        output: &mut impl Write,
        on_notification: impl FnMut(Notification),
    ) -> Result<Option<usize>, AgUiSseFeedError> {
        if self.stream.sealed() {
            self.stream.finish_replay()?;
            return Ok(None);
        }
        if self.failed {
            return Err(AgUiSseError::Closed.into());
        }
        let result = self.write_page(client, output, on_notification);
        self.failed = result.is_err();
        result
    }

    fn write_page<R: BufRead, W: Write>(
        &mut self,
        client: &mut Client<R, W>,
        output: &mut impl Write,
        on_notification: impl FnMut(Notification),
    ) -> Result<Option<usize>, AgUiSseFeedError> {
        let Some(page) =
            self.feed
                .next_page(client, PAGE_EVENTS, Some(PAGE_BYTES), on_notification)?
        else {
            self.stream.finish_replay()?;
            return Ok(None);
        };
        let mut frames = 0;
        for event in page {
            frames += self.stream.write_event(event, output)?;
            if self.stream.sealed() {
                self.stream.finish_replay()?;
                break;
            }
        }
        Ok(Some(frames))
    }
}
