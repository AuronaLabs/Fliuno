//! Read-only search within a snapshot of completed history.
use super::{Error, Kernel, Result};
use fliuno_protocol::{
    ContextMessage, SessionHistoryMatch, SessionHistoryRole, SessionHistorySearchPage,
    SessionHistorySearchParams, WorkspaceHistorySearchParams,
};

const MAX_QUERY_BYTES: usize = 256;
pub(crate) const MAX_SEARCH_TURNS: u32 = 128;
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

pub(super) fn matches_query(
    messages: &[ContextMessage],
    lowercase_query: &str,
    role: Option<SessionHistoryRole>,
) -> bool {
    messages.iter().any(|message| {
        let (text, message_role) = match message {
            ContextMessage::User { text } => (text, SessionHistoryRole::User),
            ContextMessage::Assistant { text } => (text, SessionHistoryRole::Assistant),
            _ => return false,
        };
        role.is_none_or(|filter| filter == message_role)
            && (text.contains(lowercase_query) || text.to_lowercase().contains(lowercase_query))
    })
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
            params.through_sequence,
            params.before_sequence,
            params.limit,
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
            params.through_sequence,
            params.before_sequence,
            params.limit,
        )
    }

    fn search_history(
        &self,
        scope: HistoryScope<'_>,
        query: &str,
        role: Option<SessionHistoryRole>,
        through_sequence: Option<u64>,
        before_sequence: Option<u64>,
        limit: Option<u32>,
    ) -> Result<SessionHistorySearchPage> {
        let query = validate_query(query)?;
        let limit = limit.unwrap_or(16);
        if !(1..=32).contains(&limit) {
            return Err(Error::Invalid("history limit must be 1 to 32".into()));
        }
        match scope {
            HistoryScope::Session(id) => self.require_session(id)?,
            HistoryScope::Workspace(id) => self.require_workspace(id)?,
        }
        let watermark = self.event_watermark()?;
        let through_sequence = through_sequence.unwrap_or(watermark);
        if through_sequence > watermark {
            return Err(Error::Invalid(
                "throughSequence exceeds the current event sequence".into(),
            ));
        }
        let upper = match before_sequence {
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
        page.next_before_sequence =
            self.scan_conversation(scope, upper, |sequence, turn, _, workspace_session| {
                page.scanned_turns += 1;
                if let Some(turn) = turn {
                    if matches_query(&turn.messages, &query, role) {
                        page.matches.push(SessionHistoryMatch {
                            task_id: turn.task_id,
                            completion_sequence: sequence,
                            session_id: workspace_session.map(str::to_owned),
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
