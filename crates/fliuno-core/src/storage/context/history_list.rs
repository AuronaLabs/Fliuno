//! Client history in completion order over the existing compact conversation projection.
use super::*;
use crate::json_size;

// Both history tables store ContextTurn using serde_json::to_string. The
// wire entry wraps that same compact object with a fixed tag and field name.
const TURN_ENTRY_OVERHEAD: usize = r#"{"kind":"turn","turn":}"#.len();
const ARRAY_BRACKETS: usize = 2;

impl Kernel {
    /// Fetch a single completed conversation without opening the full task snapshot.
    pub fn get_session_history_turn(
        &self,
        request: &SessionHistoryGetParams,
    ) -> Result<SessionHistoryEntry> {
        let max_bytes = history_budget(request.max_bytes)?;
        let watermark = self.event_watermark()?;
        let through = request.through_sequence.unwrap_or(watermark);
        if through > watermark {
            return Err(Error::Invalid(
                "throughSequence exceeds the current event sequence".into(),
            ));
        }
        let projection =
            conversation_projection("?4", ", s.task_id, s.sequence", "session_turns s");
        let sql = format!("{projection} WHERE s.session_id=?1 AND s.task_id=?2 AND s.sequence<=?3");
        let mut statement = self.db.prepare_cached(&sql)?;
        let mut rows = statement.query(params![
            request.session_id,
            request.task_id,
            validate_event_cursor(through)?,
            i64::from(max_bytes) - TURN_ENTRY_OVERHEAD as i64,
        ])?;
        let row = rows
            .next()?
            .ok_or_else(|| Error::NotFound("completed conversation turn".into()))?;
        let (entry, _) = decode_entry(row, max_bytes as usize)?
            .ok_or_else(|| Error::Invalid("history entry exceeds maxBytes".into()))?;
        Ok(entry)
    }

    /// List completed conversation turns in descending completion order.
    pub fn list_session_history(
        &self,
        request: &SessionHistoryListParams,
    ) -> Result<SessionHistoryListPage> {
        let limit = request.limit.unwrap_or(16);
        if !(1..=MAX_SESSION_HISTORY_ENTRIES).contains(&limit) {
            return Err(Error::Invalid("history limit must be 1 to 32".into()));
        }
        let max_bytes = history_budget(request.max_bytes)?;
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
        let after = request.after_sequence.unwrap_or(0);
        if after > through_sequence {
            return Err(Error::Invalid(
                "afterSequence exceeds throughSequence".into(),
            ));
        }
        let upper = match request.before_sequence {
            Some(before) if before > after && before <= through_sequence => before - 1,
            Some(_) => {
                return Err(Error::Invalid(
                    "beforeSequence must be above afterSequence and within throughSequence".into(),
                ));
            }
            None => through_sequence,
        };

        let projection =
            conversation_projection("?3", ", s.task_id, s.sequence", "session_turns s");
        let sql = format!(
            "{projection} WHERE s.session_id=?1 AND s.sequence<=?2 AND s.sequence>?5 ORDER BY s.sequence DESC LIMIT ?4"
        );
        let mut statement = self.db.prepare_cached(&sql)?;
        let mut rows = statement.query(params![
            request.session_id,
            validate_event_cursor(upper)?,
            i64::from(max_bytes) - (ARRAY_BRACKETS + TURN_ENTRY_OVERHEAD) as i64,
            limit + 1,
            validate_event_cursor(after)?,
        ])?;
        let mut page = SessionHistoryListPage {
            through_sequence,
            entries: Vec::with_capacity(limit as usize),
            next_before_sequence: None,
        };
        let mut last_sequence = None;
        let mut remaining = max_bytes as usize - ARRAY_BRACKETS;
        while let Some(row) = rows.next()? {
            if page.entries.len() == limit as usize {
                page.next_before_sequence = last_sequence;
                break;
            }
            let sequence = row.get::<_, i64>(2)? as u64;
            let comma = usize::from(last_sequence.is_some());
            let Some((entry, entry_bytes)) = decode_entry(row, remaining.saturating_sub(comma))?
            else {
                page.next_before_sequence = last_sequence;
                break;
            };
            page.entries.push(entry);
            remaining -= comma + entry_bytes;
            last_sequence = Some(sequence);
        }
        Ok(page)
    }
}

fn history_budget(max_bytes: Option<u32>) -> Result<u32> {
    let max_bytes = max_bytes.unwrap_or(64 * 1024);
    if !(1024..=MAX_SESSION_HISTORY_BYTES).contains(&max_bytes) {
        return Err(Error::Invalid(
            "history maxBytes must be 1024 to 262144".into(),
        ));
    }
    Ok(max_bytes)
}

fn decode_entry(
    row: &rusqlite::Row<'_>,
    remaining: usize,
) -> Result<Option<(SessionHistoryEntry, usize)>> {
    let value = row.get_ref(0)?;
    let data = value.as_str_or_null().map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(0, value.data_type(), Box::new(error))
    })?;
    if let Some(data) = data {
        let bytes = data.len() + TURN_ENTRY_OVERHEAD;
        if bytes > remaining {
            return Ok(None);
        }
        return Ok(Some((
            SessionHistoryEntry::Turn {
                turn: serde_json::from_str(data)?,
            },
            bytes,
        )));
    }
    // The SQL projection omits bodies that cannot fit even on an empty page.
    let entry = oversized(row, row.get::<_, i64>(2)? as u64)?;
    let bytes = json_size(&entry)?;
    Ok((bytes <= remaining).then_some((entry, bytes)))
}

fn oversized(row: &rusqlite::Row<'_>, completion_sequence: u64) -> Result<SessionHistoryEntry> {
    Ok(SessionHistoryEntry::Oversized {
        task_id: row.get(1)?,
        completion_sequence,
    })
}
