//! Validate and decode bounded terminal output without owning the transport or PTY.
use super::{Client, Error, Notification};
use base64::{Engine, engine::general_purpose::STANDARD};
use fliuno_protocol::{
    MAX_TERMINAL_OUTPUT_BYTES, TerminalOutput, TerminalOutputNotification, TerminalOutputParams,
    TerminalSnapshot, TerminalSubscribeParams, TerminalSubscription, TerminalSubscriptionEnded,
    TerminalTailParams, TerminalUnsubscribeParams,
};
use std::io::{BufRead, Write};

/// Validated bytes from the half-open interval `from..next`. Feed `bytes` to
/// a terminal emulator or streaming decoder; UTF-8 can span page boundaries.
#[derive(Debug)]
pub struct TerminalOutputPage {
    pub snapshot: TerminalSnapshot,
    pub from: u64,
    pub next: u64,
    pub gap: bool,
    pub bytes: Vec<u8>,
}

#[derive(Debug)]
pub enum TerminalFeedEvent {
    Output(TerminalOutputPage),
    Ended(TerminalSubscriptionEnded),
    Unrelated(Notification),
}

/// Position of one connection-local terminal subscription. Reconnect with a
/// fresh feed and the last cursor the application successfully consumed.
#[derive(Debug)]
pub struct TerminalFeed {
    identity: TerminalSubscription,
    limit: usize,
    ended: bool,
}

impl TerminalFeed {
    pub fn subscription_id(&self) -> &str {
        &self.identity.subscription_id
    }

    /// Last validated byte position; persist it only after consuming the page.
    pub fn cursor(&self) -> u64 {
        self.identity.after
    }

    /// Route in wire order, including notifications delivered during RPCs.
    /// Malformed matching frames fail without advancing the cursor. If a frame
    /// is lost or invalid, create a new subscription from the consumed cursor.
    pub fn observe(&mut self, note: Notification) -> Result<TerminalFeedEvent, Error> {
        match note.method.as_str() {
            "notifications/terminalOutput" | "notifications/terminalSubscriptionEnded"
                if note.params["subscriptionId"].as_str() == Some(self.subscription_id()) => {}
            _ => return Ok(TerminalFeedEvent::Unrelated(note)),
        }
        if self.ended {
            return Err(Error::InvalidTerminalSubscription);
        }
        if note.method == "notifications/terminalOutput" {
            let frame: TerminalOutputNotification =
                serde_json::from_value(note.params).map_err(|_| Error::InvalidTerminalOutput)?;
            let page = decode(
                frame.output,
                &self.identity.session_id,
                &self.identity.terminal_id,
                Some(self.cursor()),
                self.limit,
            )?;
            self.identity.after = page.next;
            return Ok(TerminalFeedEvent::Output(page));
        }
        let end: TerminalSubscriptionEnded =
            serde_json::from_value(note.params).map_err(|_| Error::InvalidTerminalSubscription)?;
        if end.session_id != self.identity.session_id
            || end.terminal_id != self.identity.terminal_id
            || end.after != self.cursor()
        {
            return Err(Error::InvalidTerminalSubscription);
        }
        self.ended = true;
        Ok(TerminalFeedEvent::Ended(end))
    }
}

impl<R: BufRead, W: Write> Client<R, W> {
    pub fn terminal_output(
        &mut self,
        params: &TerminalOutputParams,
        on_notification: impl FnMut(Notification),
    ) -> Result<TerminalOutputPage, Error> {
        validate_request(params.after, params.limit)?;
        let output = self.call("terminal/output", params, on_notification)?;
        decode(
            output,
            &params.session_id,
            &params.terminal_id,
            Some(params.after),
            params.limit,
        )
    }

    /// Read the last bounded page without opening a live subscription. Use the
    /// page's `next` in `subscribe_terminal` after consuming these bytes.
    pub fn terminal_tail(
        &mut self,
        params: &TerminalTailParams,
        on_notification: impl FnMut(Notification),
    ) -> Result<TerminalOutputPage, Error> {
        validate_request(0, params.limit)?;
        let output = self.call("terminal/tail", params, on_notification)?;
        decode(
            output,
            &params.session_id,
            &params.terminal_id,
            None,
            params.limit,
        )
    }

    /// Begin a new observation after `params.after`. Any notifications already
    /// emitted by other subscriptions are passed to `on_notification`.
    pub fn subscribe_terminal(
        &mut self,
        params: &TerminalSubscribeParams,
        on_notification: impl FnMut(Notification),
    ) -> Result<TerminalFeed, Error> {
        validate_request(params.after, params.limit)?;
        let identity: TerminalSubscription =
            self.call("terminal/subscribe", params, on_notification)?;
        if identity.subscription_id.is_empty()
            || identity.session_id != params.session_id
            || identity.terminal_id != params.terminal_id
            || identity.after != params.after
        {
            return Err(Error::InvalidTerminalSubscription);
        }
        Ok(TerminalFeed {
            identity,
            limit: params.limit,
            ended: false,
        })
    }

    /// Stop observing on this connection. This does not stop the terminal.
    pub fn unsubscribe_terminal(
        &mut self,
        subscription_id: &str,
        on_notification: impl FnMut(Notification),
    ) -> Result<bool, Error> {
        let params = TerminalUnsubscribeParams {
            subscription_id: subscription_id.into(),
        };
        #[derive(serde::Deserialize)]
        struct ResultValue {
            removed: bool,
        }
        let result: ResultValue = self.call("terminal/unsubscribe", &params, on_notification)?;
        Ok(result.removed)
    }
}

fn validate_request(after: u64, limit: usize) -> Result<(), Error> {
    if !(1..=MAX_TERMINAL_OUTPUT_BYTES).contains(&limit) || after > i64::MAX as u64 {
        return Err(Error::InvalidTerminalCursor);
    }
    Ok(())
}

fn decode(
    output: TerminalOutput,
    session: &str,
    terminal: &str,
    after: Option<u64>,
    limit: usize,
) -> Result<TerminalOutputPage, Error> {
    let TerminalOutput {
        snapshot,
        from,
        next,
        gap,
        data_base64,
    } = output;
    let (start, end) = (snapshot.start_offset, snapshot.end_offset);
    let expected_from = after
        .unwrap_or_else(|| end.saturating_sub(limit as u64))
        .max(start);
    let expected_gap = after.map_or(from != 0, |cursor| cursor < start);
    if snapshot.session_id != session
        || snapshot.terminal_id != terminal
        || start > end
        || end > i64::MAX as u64
        || from != expected_from
        || gap != expected_gap
        || next < from
        || next > end
        || (next == from && next != end)
        || (after.is_none() && next != end)
        || next - from > limit as u64
        || data_base64.len() != ((next - from) as usize).div_ceil(3) * 4
    {
        return Err(Error::InvalidTerminalOutput);
    }
    let bytes = STANDARD
        .decode(data_base64)
        .map_err(|_| Error::InvalidTerminalOutput)?;
    if bytes.len() as u64 != next - from {
        return Err(Error::InvalidTerminalOutput);
    }
    Ok(TerminalOutputPage {
        snapshot,
        from,
        next,
        gap,
        bytes,
    })
}

#[cfg(test)]
mod tests;
