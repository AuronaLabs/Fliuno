//! Independent SSE replay cursors for hosts sharing one stdio connection.
use super::{
    AgUiRunPhase, AgUiSseError, AgUiSseFeedError, AgUiSseStream,
    stream::{PAGE_BYTES, PAGE_EVENTS},
};
use crate::{Client, Error, EventReplay, Notification, ReplayScope};
use fliuno_protocol::TaskSummary;
use std::io::{BufRead, Write};

/// Encodes one task's committed events without modifying the live subscription.
/// A host can interleave readers and send each encoded chunk after releasing
/// the client. Use a shared watermark to advance readers as new events commit.
pub struct AgUiSseReplay {
    replay: EventReplay,
    stream: AgUiSseStream,
    failed: bool,
}

impl AgUiSseReplay {
    /// Start from a committed watermark, for example from `Client::event_watermark`.
    /// Use `AgUiRun::replay_sse` to preserve a run ID supplied by an AG-UI client.
    pub fn new(
        task: &TaskSummary,
        through: u64,
        last_event_id: Option<&str>,
    ) -> Result<Self, AgUiSseFeedError> {
        Self::with_stream(AgUiSseStream::new(task, last_event_id)?, through)
    }

    pub(super) fn with_stream(
        stream: AgUiSseStream,
        through: u64,
    ) -> Result<Self, AgUiSseFeedError> {
        let replay = EventReplay::new(ReplayScope::Task(stream.task_id().into()), 0, through)?;
        stream.check_watermark(through)?;
        Ok(Self {
            replay,
            stream,
            failed: false,
        })
    }

    /// Extend the current replay window after consuming it with `next_chunk`.
    pub fn advance_through(&mut self, through: u64) -> Result<(), AgUiSseFeedError> {
        if self.failed {
            return Err(AgUiSseError::Closed.into());
        }
        let after = self.replay.cursor();
        if after != self.replay.through() || through < after {
            return Err(Error::InvalidReplayWindow.into());
        }
        self.stream
            .finish_replay()
            .inspect_err(|_| self.failed = true)?;
        self.replay = EventReplay::new(
            ReplayScope::Task(self.stream.task_id().into()),
            after,
            through,
        )?;
        Ok(())
    }

    pub fn run_phase(&self) -> Option<AgUiRunPhase> {
        (!self.failed).then(|| self.stream.run_phase())
    }

    /// Encode a page. `Some` may be empty for skipped events. `None` confirms
    /// the current window is complete and any resume ID was found in it.
    pub fn next_chunk<R: BufRead, W: Write>(
        &mut self,
        client: &mut Client<R, W>,
        on_notification: impl FnMut(Notification),
    ) -> Result<Option<Vec<u8>>, AgUiSseFeedError> {
        if self.failed {
            return Err(AgUiSseError::Closed.into());
        }
        let result = self.encode_page(client, on_notification);
        self.failed = result.is_err();
        result
    }

    fn encode_page<R: BufRead, W: Write>(
        &mut self,
        client: &mut Client<R, W>,
        on_notification: impl FnMut(Notification),
    ) -> Result<Option<Vec<u8>>, AgUiSseFeedError> {
        let Some(page) =
            self.replay
                .next_page(client, PAGE_EVENTS, Some(PAGE_BYTES), on_notification)?
        else {
            self.stream.finish_replay()?;
            return Ok(None);
        };
        let mut output = Vec::new();
        for event in page {
            self.stream.write_event(event, &mut output)?;
        }
        Ok(Some(output))
    }
}
