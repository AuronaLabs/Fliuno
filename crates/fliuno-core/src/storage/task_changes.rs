//! Replay ordered task invalidations without reading any event bodies.
use super::*;
use std::collections::HashMap;

impl Kernel {
    pub fn list_task_changes(&self, request: &TaskChangesParams) -> Result<TaskChangesPage> {
        let (sql, owner) = match (&request.task_id, &request.session_id, &request.workspace_id) {
            (None, None, None) => (
                "SELECT sequence,task_id FROM events
                 WHERE ?1 IS NULL AND sequence>?2 AND sequence<=?3 ORDER BY sequence LIMIT ?4",
                None,
            ),
            (Some(id), None, None) => {
                self.require_task(id)?;
                (
                    "SELECT sequence,task_id FROM events
                     WHERE task_id=?1 AND sequence>?2 AND sequence<=?3 ORDER BY sequence LIMIT ?4",
                    Some(id.as_str()),
                )
            }
            (None, Some(id), None) => {
                self.require_session(id)?;
                (
                    "SELECT i.sequence,e.task_id FROM session_event_index i
                     CROSS JOIN events e ON e.sequence=i.sequence
                     WHERE i.session_id=?1 AND i.sequence>?2 AND i.sequence<=?3
                     ORDER BY i.sequence LIMIT ?4",
                    Some(id.as_str()),
                )
            }
            (None, None, Some(id)) => {
                self.require_workspace(id)?;
                (
                    "SELECT i.sequence,e.task_id FROM workspace_event_index i
                     CROSS JOIN events e ON e.sequence=i.sequence
                     WHERE i.workspace_id=?1 AND i.sequence>?2 AND i.sequence<=?3
                     ORDER BY i.sequence LIMIT ?4",
                    Some(id.as_str()),
                )
            }
            _ => return Err(Error::Invalid("select one task change scope".into())),
        };
        let fetch = replay::validate_event_limit(request.limit)?;
        let now = self.event_watermark()?;
        let through = request.through.unwrap_or(now);
        if request.after > through || through > now {
            return Err(Error::Invalid("invalid task change replay window".into()));
        }
        let mut stmt = self.db.prepare_cached(sql)?;
        let mut rows = stmt.query(params![owner, request.after as i64, through as i64, fetch])?;
        let mut last_by_task = HashMap::new();
        let mut last = request.after;
        let mut has_more = false;
        let mut scanned = 0;
        while let Some(row) = rows.next()? {
            // Probe for another match without including it in this page.
            if scanned == request.limit {
                has_more = true;
                break;
            }
            scanned += 1;
            last = row.get::<_, i64>(0)? as u64;
            let id = row.get_ref(1)?.as_str().map_err(rusqlite::Error::from)?;
            if let Some(sequence) = last_by_task.get_mut(id) {
                *sequence = last;
            } else {
                last_by_task.insert(id.to_owned(), last);
            }
        }
        let mut changes: Vec<_> = last_by_task
            .into_iter()
            .map(|(task_id, sequence)| TaskChange { task_id, sequence })
            .collect();
        changes.sort_unstable_by_key(|change| change.sequence);
        let next_after = if has_more { last } else { through };
        Ok(TaskChangesPage {
            changes,
            next_after,
            through_sequence: through,
            has_more,
        })
    }
}
