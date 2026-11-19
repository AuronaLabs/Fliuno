//! Replay ordered task invalidations without reading any event bodies.
use super::*;
use std::collections::HashMap;

impl Kernel {
    pub fn list_task_changes(&self, request: &TaskChangesParams) -> Result<TaskChangesPage> {
        self.read_task_changes(request).map(|(page, _)| page)
    }

    /// Extend a bounded replay page with current task summaries in one read view.
    pub fn list_task_summary_changes(
        &self,
        request: TaskSummaryChangesParams,
    ) -> Result<TaskSummaryChangesPage> {
        if !(1..=MAX_TASK_SUMMARY_CHANGE_EVENTS).contains(&request.limit) {
            return Err(Error::Invalid(format!(
                "limit must be 1 to {MAX_TASK_SUMMARY_CHANGE_EVENTS}"
            )));
        }
        // All reads use this connection; BEGIN pins the first read snapshot
        // through both invalidation selection and summary projection lookup.
        let tx = self.db.unchecked_transaction()?;
        let (page, summary_through_sequence) = self.read_task_changes(&TaskChangesParams {
            task_id: request.task_id,
            session_id: request.session_id,
            workspace_id: request.workspace_id,
            after: request.after,
            through: request.through,
            limit: request.limit,
        })?;
        let mut changes = Vec::with_capacity(page.changes.len());
        if !page.changes.is_empty() {
            let mut stmt = tx.prepare_cached("SELECT data FROM task_summaries WHERE task_id=?1")?;
            for change in page.changes {
                let mut rows = stmt.query([change.task_id])?;
                let row = rows.next()?.ok_or(rusqlite::Error::QueryReturnedNoRows)?;
                let data = row.get_ref(0)?.as_str().map_err(rusqlite::Error::from)?;
                changes.push(TaskSummaryChange {
                    sequence: change.sequence,
                    summary: serde_json::from_str(data)?,
                });
            }
        }
        tx.commit()?;
        Ok(TaskSummaryChangesPage {
            changes,
            next_after: page.next_after,
            through_sequence: page.through_sequence,
            has_more: page.has_more,
            summary_through_sequence,
        })
    }

    fn read_task_changes(&self, request: &TaskChangesParams) -> Result<(TaskChangesPage, u64)> {
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
        Ok((
            TaskChangesPage {
                changes,
                next_after,
                through_sequence: through,
                has_more,
            },
            now,
        ))
    }
}
