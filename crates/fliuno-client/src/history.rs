//! Walk a fixed view of completed session history over the local RPC stream.
use super::{Client, Error, EventFeed, EventReplay, Notification, ReplayScope};
use fliuno_protocol::{
    ContextMessage, SessionHistoryEntry, SessionHistoryGetParams, SessionHistoryListPage,
    SessionHistoryListParams, SessionHistorySubscribeParams,
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
        if self.request.after_sequence.is_some()
            && client
                .info()
                .capabilities
                .get("sessionHistoryAfterSequence")
                .and_then(serde_json::Value::as_bool)
                != Some(true)
        {
            return Err(Error::UnsupportedSessionHistoryAfterSequence);
        }
        let page: SessionHistoryListPage =
            client.call("session/listHistory", &self.request, on_notification)?;
        self.accept_page(&page)?;
        Ok(Some(page))
    }

    fn accept_page(&mut self, page: &SessionHistoryListPage) -> Result<(), Error> {
        let through = page.through_sequence;
        let after = self.request.after_sequence.unwrap_or(0);
        if through > i64::MAX as u64
            || after > through
            || self
                .request
                .through_sequence
                .is_some_and(|expected| expected != through)
            || page.entries.len() > self.request.limit.unwrap_or(16) as usize
        {
            return Err(Error::InvalidHistoryPage);
        }
        let mut previous = self.request.before_sequence;
        for entry in &page.entries {
            let (_, sequence) = history_identity(entry)?;
            if sequence <= after
                || sequence > through
                || previous.is_some_and(|prev| sequence >= prev)
            {
                return Err(Error::InvalidHistoryPage);
            }
            previous = Some(sequence);
        }
        if page.next_before_sequence.is_some_and(|next| {
            page.entries.is_empty() || previous != Some(next) || next <= after + 1
        }) {
            return Err(Error::InvalidHistoryPage);
        }
        self.request.through_sequence = Some(through);
        self.request.before_sequence = page.next_before_sequence;
        self.exhausted = page.next_before_sequence.is_none();
        Ok(())
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

/// First history page, remaining pages at the same watermark, and newer events.
pub struct SessionHistorySubscription {
    pub page: SessionHistoryListPage,
    pub history: SessionHistory,
    pub events: EventFeed,
}

impl<R: BufRead, W: Write> Client<R, W> {
    /// Read the current history page and switch the live subscription in one RPC.
    /// Notifications received before the response belong to the previous subscription.
    pub fn subscribe_session_history(
        &mut self,
        request: SessionHistorySubscribeParams,
        on_notification: impl FnMut(Notification),
    ) -> Result<SessionHistorySubscription, Error> {
        if self
            .info()
            .capabilities
            .get("sessionHistorySubscription")
            .and_then(serde_json::Value::as_bool)
            != Some(true)
        {
            return Err(Error::UnsupportedSessionHistorySubscription);
        }
        let page: SessionHistoryListPage =
            self.call("session/subscribeHistory", &request, on_notification)?;
        let scope = ReplayScope::Session(request.session_id.clone());
        let mut history = SessionHistory::new(request.into());
        history.accept_page(&page)?;
        let through = page.through_sequence;
        let events = EventFeed::from_replay(EventReplay::new(scope, through, through)?);
        Ok(SessionHistorySubscription {
            page,
            history,
            events,
        })
    }

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
