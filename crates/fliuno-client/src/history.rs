//! Walk a fixed view of completed session history over the local RPC stream.
use super::{Client, Error, Notification};
use fliuno_protocol::{
    ContextMessage, SessionHistoryEntry, SessionHistoryGetParams, SessionHistoryListPage,
    SessionHistoryListParams,
};
use std::io::{BufRead, Write};

#[derive(Debug)]
pub struct SessionHistory {
    request: SessionHistoryListParams,
    exhausted: bool,
}

impl SessionHistory {
    /// Start a new listing or resume from a saved `next_request()`.
    pub fn new(request: SessionHistoryListParams) -> Self {
        Self {
            request,
            exhausted: false,
        }
    }

    /// The fixed event watermark, available after the first successful request.
    pub fn through(&self) -> Option<u64> {
        self.request.through_sequence
    }

    /// Save this request with the last page consumed to continue after restart.
    pub fn next_request(&self) -> Option<&SessionHistoryListParams> {
        (!self.exhausted).then_some(&self.request)
    }

    /// Fetch one page, including the first empty page if the session has no history.
    /// A failed request leaves the cursor unchanged.
    pub fn next_page<R: BufRead, W: Write>(
        &mut self,
        client: &mut Client<R, W>,
        on_notification: impl FnMut(Notification),
    ) -> Result<Option<SessionHistoryListPage>, Error> {
        if self.exhausted {
            return Ok(None);
        }
        let page: SessionHistoryListPage =
            client.call("session/listHistory", &self.request, on_notification)?;
        if page.through_sequence > i64::MAX as u64
            || self
                .request
                .through_sequence
                .is_some_and(|through| through != page.through_sequence)
            || page.entries.len() > self.request.limit.unwrap_or(16) as usize
        {
            return Err(Error::InvalidHistoryPage);
        }
        let mut previous = self.request.before_sequence;
        for entry in &page.entries {
            let (_, sequence) = history_identity(entry)?;
            if sequence == 0
                || sequence > page.through_sequence
                || previous.is_some_and(|prev| sequence >= prev)
            {
                return Err(Error::InvalidHistoryPage);
            }
            previous = Some(sequence);
        }
        if page
            .next_before_sequence
            .is_some_and(|next| page.entries.is_empty() || previous != Some(next) || next <= 1)
        {
            return Err(Error::InvalidHistoryPage);
        }
        self.request.through_sequence = Some(page.through_sequence);
        self.request.before_sequence = page.next_before_sequence;
        self.exhausted = page.next_before_sequence.is_none();
        Ok(Some(page))
    }
}

pub(super) fn history_identity(entry: &SessionHistoryEntry) -> Result<(&str, u64), Error> {
    let (id, sequence) = match entry {
        SessionHistoryEntry::Turn { turn } => {
            if !turn.host_tools.is_empty()
                || turn.messages.iter().any(|message| {
                    !matches!(
                        message,
                        ContextMessage::User { .. } | ContextMessage::Assistant { .. }
                    )
                })
            {
                return Err(Error::InvalidHistoryPage);
            }
            (turn.task_id.as_str(), turn.completion_sequence)
        }
        SessionHistoryEntry::Oversized {
            task_id,
            completion_sequence,
        } => (task_id.as_str(), *completion_sequence),
    };
    if id.is_empty() || !(1..=i64::MAX as u64).contains(&sequence) {
        return Err(Error::InvalidHistoryPage);
    }
    Ok((id, sequence))
}

impl<R: BufRead, W: Write> Client<R, W> {
    /// Read one bounded, compact conversation entry in the selected session.
    pub fn get_history_turn(
        &mut self,
        request: &SessionHistoryGetParams,
        on_notification: impl FnMut(Notification),
    ) -> Result<SessionHistoryEntry, Error> {
        let entry = self.call("session/getHistoryTurn", request, on_notification)?;
        let (id, sequence) = history_identity(&entry)?;
        if id != request.task_id
            || request
                .through_sequence
                .is_some_and(|through| sequence > through)
        {
            return Err(Error::InvalidHistoryPage);
        }
        Ok(entry)
    }
}
