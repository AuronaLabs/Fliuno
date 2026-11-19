//! Client history in completion order over the existing compact conversation projection.
use super::*;
use crate::json_size;

impl Kernel {
    /// List completed conversation turns in descending completion order.
    pub fn list_session_history(
        &self,
        request: &SessionHistoryListParams,
    ) -> Result<SessionHistoryListPage> {
        let limit = request.limit.unwrap_or(16);
        if !(1..=MAX_SESSION_HISTORY_ENTRIES).contains(&limit) {
            return Err(Error::Invalid("history limit must be 1 to 32".into()));
        }
        let max_bytes = request.max_bytes.unwrap_or(64 * 1024);
        if !(1024..=MAX_SESSION_HISTORY_BYTES).contains(&max_bytes) {
            return Err(Error::Invalid(
                "history maxBytes must be 1024 to 262144".into(),
            ));
        }
        if request.before_sequence.is_some() && request.through_sequence.is_none() {
            return Err(Error::Invalid(
                "beforeSequence requires throughSequence".into(),
            ));
        }
        self.require_session(&request.session_id)?;
        let watermark = self.event_watermark()?;
        let through_sequence = request.through_sequence.unwrap_or(watermark);
        if through_sequence > watermark {
            return Err(Error::Invalid(
                "throughSequence exceeds the current event sequence".into(),
            ));
        }
        let upper = match request.before_sequence {
            Some(before) if before > 0 && before <= through_sequence => before - 1,
            Some(_) => {
                return Err(Error::Invalid(
                    "beforeSequence must be within throughSequence".into(),
                ));
            }
            None => through_sequence,
        };

        let projection =
            conversation_projection("?3", ", s.task_id, s.sequence", "session_turns s");
        let sql = format!(
            "{projection} WHERE s.session_id=?1 AND s.sequence<=?2 ORDER BY s.sequence DESC LIMIT ?4"
        );
        let mut statement = self.db.prepare_cached(&sql)?;
        let mut rows = statement.query(params![
            request.session_id,
            validate_event_cursor(upper)?,
            max_bytes,
            limit + 1,
        ])?;
        let mut page = SessionHistoryListPage {
            through_sequence,
            entries: Vec::with_capacity(limit as usize),
            next_before_sequence: None,
        };
        let mut last_sequence = None;
        let mut bytes = 2; // JSON array brackets.
        while let Some(row) = rows.next()? {
            if page.entries.len() == limit as usize {
                page.next_before_sequence = last_sequence;
                break;
            }
            let sequence = row.get::<_, i64>(2)? as u64;
            let value = row.get_ref(0)?;
            let data = value.as_str_or_null().map_err(|error| {
                rusqlite::Error::FromSqlConversionFailure(0, value.data_type(), Box::new(error))
            })?;
            let mut entry = match data {
                Some(data) => SessionHistoryEntry::Turn {
                    turn: serde_json::from_str(data)?,
                },
                None => oversized(row, sequence)?,
            };
            let mut entry_bytes = json_size(&entry)?;
            if entry_bytes + 2 > max_bytes as usize {
                entry = oversized(row, sequence)?;
                entry_bytes = json_size(&entry)?;
            }
            let next_bytes = bytes + usize::from(last_sequence.is_some()) + entry_bytes;
            if next_bytes > max_bytes as usize {
                page.next_before_sequence = last_sequence;
                break;
            }
            page.entries.push(entry);
            bytes = next_bytes;
            last_sequence = Some(sequence);
        }
        Ok(page)
    }
}

fn oversized(row: &rusqlite::Row<'_>, completion_sequence: u64) -> Result<SessionHistoryEntry> {
    Ok(SessionHistoryEntry::Oversized {
        task_id: row.get(1)?,
        completion_sequence,
    })
}
