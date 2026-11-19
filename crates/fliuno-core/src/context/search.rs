//! Read-only search within a snapshot of completed history.
use super::{Error, Kernel, Result};
use fliuno_protocol::{
    MAX_HISTORY_SEARCH_SCAN_LIMIT, SessionHistoryMatch, SessionHistoryRole,
    SessionHistorySearchPage, SessionHistorySearchParams, WorkspaceHistorySearchParams,
};

use serde::Deserialize;
use std::borrow::Cow;

const MAX_QUERY_BYTES: usize = 256;
pub(crate) use fliuno_protocol::MAX_HISTORY_SEARCH_TURNS as DEFAULT_SEARCH_TURNS;
pub(crate) const MAX_SEARCH_TURN_BYTES: u32 = 128 * 1024;

#[derive(Clone, Copy)]
pub(crate) enum HistoryScope<'a> {
    Session(&'a str),
    Workspace(&'a str),
}

pub(super) fn validate_query(query: &str) -> Result<&str> {
    let query = query.trim();
    if query.is_empty() || query.len() > MAX_QUERY_BYTES {
        return Err(Error::Invalid(
            "history query must be 1 to 256 UTF-8 bytes".into(),
        ));
    }
    Ok(query)
}

/// Match only user and assistant text. Borrow unescaped strings directly
/// from the row; serde only allocates text that actually needs JSON unescaping.
#[derive(Deserialize)]
struct SearchDocument<'a> {
    #[serde(borrow)]
    messages: Vec<SearchMessage<'a>>,
}

#[derive(Deserialize)]
struct SearchMessage<'a> {
    role: SearchRole,
    #[serde(borrow, default)]
    text: Cow<'a, str>,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum SearchRole {
    User,
    Assistant,
    ToolCall,
    ToolResult,
}

pub(super) struct HistoryMatcher<'a> {
    lowercase_query: &'a str,
    role: Option<SessionHistoryRole>,
}

impl<'a> HistoryMatcher<'a> {
    pub(super) fn new(lowercase_query: &'a str, role: Option<SessionHistoryRole>) -> Self {
        Self {
            lowercase_query,
            role,
        }
    }

    pub(super) fn matches(&self, data: &str) -> Result<bool> {
        let doc: SearchDocument<'_> = serde_json::from_str(data)?;
        Ok(doc.messages.into_iter().any(|message| {
            let role = match message.role {
                SearchRole::User => SessionHistoryRole::User,
                SearchRole::Assistant => SessionHistoryRole::Assistant,
                SearchRole::ToolCall | SearchRole::ToolResult => return false,
            };
            self.role.is_none_or(|filter| filter == role) && self.matches_text(&message.text)
        }))
    }

    fn matches_text(&self, text: &str) -> bool {
        // Keep the original matching behavior, including Rust's contextual
        // Unicode lowercase mappings (for example, Greek final sigma).
        text.contains(self.lowercase_query) || text.to_lowercase().contains(self.lowercase_query)
    }
}

struct SearchWindow {
    through: Option<u64>,
    before: Option<u64>,
    limit: Option<u32>,
    scan_limit: Option<u32>,
}

impl Kernel {
    pub fn search_session_history(
        &self,
        params: SessionHistorySearchParams,
    ) -> Result<SessionHistorySearchPage> {
        self.search_history(
            HistoryScope::Session(&params.session_id),
            &params.query,
            params.role,
            SearchWindow {
                through: params.through_sequence,
                before: params.before_sequence,
                limit: params.limit,
                scan_limit: params.scan_limit,
            },
        )
    }

    pub fn search_workspace_history(
        &self,
        params: WorkspaceHistorySearchParams,
    ) -> Result<SessionHistorySearchPage> {
        self.search_history(
            HistoryScope::Workspace(&params.workspace_id),
            &params.query,
            params.role,
            SearchWindow {
                through: params.through_sequence,
                before: params.before_sequence,
                limit: params.limit,
                scan_limit: params.scan_limit,
            },
        )
    }

    fn search_history(
        &self,
        scope: HistoryScope<'_>,
        query: &str,
        role: Option<SessionHistoryRole>,
        window: SearchWindow,
    ) -> Result<SessionHistorySearchPage> {
        let query = validate_query(query)?;
        let limit = window.limit.unwrap_or(16);
        if !(1..=32).contains(&limit) {
            return Err(Error::Invalid("history limit must be 1 to 32".into()));
        }
        let scan_limit = window.scan_limit.unwrap_or(DEFAULT_SEARCH_TURNS);
        if !(1..=MAX_HISTORY_SEARCH_SCAN_LIMIT).contains(&scan_limit) {
            return Err(Error::Invalid(format!(
                "history scanLimit must be 1 to {MAX_HISTORY_SEARCH_SCAN_LIMIT}"
            )));
        }
        match scope {
            HistoryScope::Session(id) => self.require_session(id)?,
            HistoryScope::Workspace(id) => self.require_workspace(id)?,
        }
        let watermark = self.event_watermark()?;
        let through_sequence = window.through.unwrap_or(watermark);
        if through_sequence > watermark {
            return Err(Error::Invalid(
                "throughSequence exceeds the current event sequence".into(),
            ));
        }
        let upper = match window.before {
            Some(before) if before > 0 && before <= through_sequence => before - 1,
            Some(_) => {
                return Err(Error::Invalid(
                    "beforeSequence must be within throughSequence".into(),
                ));
            }
            None => through_sequence,
        };
        let query = query.to_lowercase();
        let mut page = SessionHistorySearchPage {
            through_sequence,
            matches: Vec::with_capacity(limit as usize),
            scanned_turns: 0,
            skipped_oversized_turns: 0,
            next_before_sequence: None,
        };
        let matcher = HistoryMatcher::new(&query, role);
        page.next_before_sequence = self.scan_conversation(scope, upper, scan_limit, |row| {
            page.scanned_turns += 1;
            if let Some(data) = row.data {
                if matcher.matches(data)? {
                    page.matches.push(SessionHistoryMatch {
                        task_id: row.task_id.to_owned(),
                        completion_sequence: row.sequence,
                        session_id: row.session_id.map(str::to_owned),
                    });
                }
            } else {
                page.skipped_oversized_turns += 1;
            }
            Ok(page.matches.len() < limit as usize)
        })?;
        Ok(page)
    }
}
