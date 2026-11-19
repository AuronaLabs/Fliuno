//! Resumable, bounded search of completed conversation history.
use crate::{Client, Error, Notification};
use fliuno_protocol::{
    MAX_HISTORY_SEARCH_SCAN_LIMIT, MAX_HISTORY_SEARCH_TURNS, SessionHistorySearchPage,
    SessionHistorySearchParams, WorkspaceHistorySearchParams,
};
use serde::{Deserialize, Serialize};
use std::io::{BufRead, Write};

/// Save this request alongside the last consumed page to resume a search.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "scope", content = "params", rename_all = "snake_case")]
pub enum HistorySearchRequest {
    Session(SessionHistorySearchParams),
    Workspace(WorkspaceHistorySearchParams),
}

#[derive(Debug)]
pub struct HistorySearch {
    next: Option<HistorySearchRequest>,
}

impl HistorySearch {
    pub fn new(request: HistorySearchRequest) -> Self {
        Self {
            next: Some(request),
        }
    }

    pub fn next_request(&self) -> Option<&HistorySearchRequest> {
        self.next.as_ref()
    }

    /// Fetch one search window. Empty matches are not exhaustion if the server
    /// returned a continuation cursor. Errors leave the request unchanged.
    pub fn next_page<R: BufRead, W: Write>(
        &mut self,
        client: &mut Client<R, W>,
        on_notification: impl FnMut(Notification),
    ) -> Result<Option<SessionHistorySearchPage>, Error> {
        let Some(request) = &mut self.next else {
            return Ok(None);
        };
        let scan_limit = match request {
            HistorySearchRequest::Session(params) => params.scan_limit,
            HistorySearchRequest::Workspace(params) => params.scan_limit,
        };
        // Preserve the requested work budget. Silently sending it to a legacy
        // server, or dropping it, would change either validation or the work.
        if let Some(limit) = scan_limit {
            if !(1..=MAX_HISTORY_SEARCH_SCAN_LIMIT).contains(&limit) {
                return Err(Error::InvalidHistorySearchScanLimit);
            }
            let advertised = client
                .info()
                .capabilities
                .get("maxHistorySearchScanLimit")
                .and_then(serde_json::Value::as_u64);
            if advertised.is_none_or(|max| u64::from(limit) > max) {
                return Err(Error::UnsupportedHistorySearchScanLimit);
            }
        }
        let (page, through, before, limit, workspace) = match request {
            HistorySearchRequest::Session(params) => {
                let page = client.call("session/searchHistory", params, on_notification)?;
                (
                    page,
                    &mut params.through_sequence,
                    &mut params.before_sequence,
                    params.limit,
                    false,
                )
            }
            HistorySearchRequest::Workspace(params) => {
                let page = client.call("workspace/searchHistory", params, on_notification)?;
                (
                    page,
                    &mut params.through_sequence,
                    &mut params.before_sequence,
                    params.limit,
                    true,
                )
            }
        };
        validate_page(
            &page,
            *through,
            *before,
            limit.unwrap_or(16),
            scan_limit.unwrap_or(MAX_HISTORY_SEARCH_TURNS),
            workspace,
        )?;
        if let Some(next) = page.next_before_sequence {
            *through = Some(page.through_sequence);
            *before = Some(next);
        } else {
            self.next = None;
        }
        Ok(Some(page))
    }
}

fn validate_page(
    page: &SessionHistorySearchPage,
    through: Option<u64>,
    before: Option<u64>,
    limit: u32,
    scan_limit: u32,
    workspace: bool,
) -> Result<(), Error> {
    if page.through_sequence > i64::MAX as u64
        || through.is_some_and(|stamp| stamp != page.through_sequence)
        || page.scanned_turns > scan_limit
        || page.skipped_oversized_turns > page.scanned_turns
        || page.matches.len() > limit as usize
        || page.matches.len() > (page.scanned_turns - page.skipped_oversized_turns) as usize
    {
        return Err(Error::InvalidHistoryPage);
    }
    let mut previous = before;
    for hit in &page.matches {
        let sequence = hit.completion_sequence;
        if sequence == 0
            || sequence > page.through_sequence
            || previous.is_some_and(|prev| sequence >= prev)
            || hit.task_id.is_empty()
            || if workspace {
                hit.session_id.as_deref().is_none_or(str::is_empty)
            } else {
                hit.session_id.is_some()
            }
        {
            return Err(Error::InvalidHistoryPage);
        }
        previous = Some(sequence);
    }
    if page.next_before_sequence.is_some_and(|next| {
        next <= 1
            || next > page.through_sequence
            || before.is_some_and(|prev| next >= prev)
            || previous.is_some_and(|seq| next > seq)
            || (page.scanned_turns != scan_limit && page.matches.len() != limit as usize)
            || (page.matches.len() == limit as usize && previous != Some(next))
    }) {
        return Err(Error::InvalidHistoryPage);
    }
    Ok(())
}
