//! Walk a bounded completion window and borrow each eligible JSON projection.
use super::*;
use crate::context::search::{HistoryScope, MAX_SEARCH_TURN_BYTES};

pub(crate) struct ConversationRow<'a> {
    pub(crate) sequence: u64,
    /// Oversized rows are represented by their cursor and IDs without JSON.
    pub(crate) data: Option<&'a str>,
    pub(crate) has_compact_projection: bool,
    pub(crate) session_id: Option<&'a str>,
    pub(crate) task_id: &'a str,
}

impl Kernel {
    pub(crate) fn scan_conversation(
        &self,
        scope: HistoryScope<'_>,
        after: u64,
        upper: u64,
        scan_limit: u32,
        mut visit: impl FnMut(ConversationRow<'_>) -> Result<bool>,
    ) -> Result<Option<u64>> {
        let (owner, source, predicate, cursor, session_column) = match scope {
            HistoryScope::Session(id) => (
                id,
                "session_context_turns s",
                "s.session_id",
                "s.sequence",
                "NULL",
            ),
            // Keep the workspace range on the outside of the join so LIMIT
            // bounds work even if unrelated workspaces have much more history.
            HistoryScope::Workspace(id) => (
                id,
                "workspace_turn_index w CROSS JOIN session_turns s ON s.sequence=w.sequence",
                "w.workspace_id",
                "w.sequence",
                "s.session_id",
            ),
        };
        let columns = format!(", s.sequence, c.task_id IS NOT NULL, {session_column}, s.task_id");
        let projection = conversation_projection("?3", &columns, source);
        let sql = format!(
            "{projection} WHERE {predicate}=?1 AND {cursor}<=?2 AND {cursor}>?5 ORDER BY {cursor} DESC LIMIT ?4"
        );
        let mut statement = self.db.prepare_cached(&sql)?;
        let mut rows = statement.query(params![
            owner,
            validate_event_cursor(upper)?,
            MAX_SEARCH_TURN_BYTES,
            scan_limit + 1,
            validate_event_cursor(after)?
        ])?;
        for index in 0..scan_limit {
            let Some(row) = rows.next()? else {
                break;
            };
            let sequence = row.get::<_, i64>(1)? as u64;
            let record = ConversationRow {
                sequence,
                data: row
                    .get_ref(0)?
                    .as_str_or_null()
                    .map_err(rusqlite::Error::from)?,
                has_compact_projection: row.get(2)?,
                session_id: row
                    .get_ref(3)?
                    .as_str_or_null()
                    .map_err(rusqlite::Error::from)?,
                task_id: row.get_ref(4)?.as_str().map_err(rusqlite::Error::from)?,
            };
            if !visit(record)? || index + 1 == scan_limit {
                // Peek without decoding another turn to distinguish a full page from exhaustion.
                return Ok(rows.next()?.is_some().then_some(sequence));
            }
        }
        Ok(None)
    }
}
