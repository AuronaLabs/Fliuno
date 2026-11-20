//! Replay ordered task invalidations without reading any event bodies.
use super::*;
use std::collections::HashMap;

impl Kernel {
    pub fn list_task_changes(&self, request: &TaskChangesParams) -> Result<TaskChangesPage> {
        self.read_task_changes(request, ChangeSource::Events)
            .map(|(page, _)| page)
    }

    /// Extend a bounded replay page with current task summaries in one read view.
    pub fn list_task_summary_changes(
        &self,
        request: TaskSummaryChangesParams,
    ) -> Result<TaskSummaryChangesPage> {
        self.read_task_summary_changes(request, ChangeSource::Events)
    }

    /// Use narrow summary revisions, with a conservative prefix for old stores.
    pub fn list_task_summary_revisions(
        &self,
        request: TaskSummaryChangesParams,
    ) -> Result<TaskSummaryChangesPage> {
        self.read_task_summary_changes(request, ChangeSource::Revisions)
    }

    /// Return each task at its last matching revision in the whole replay window.
    pub fn list_latest_task_summary_revisions(
        &self,
        request: TaskSummaryChangesParams,
    ) -> Result<TaskSummaryChangesPage> {
        self.read_task_summary_changes(request, ChangeSource::LatestRevisions)
    }

    fn read_task_summary_changes(
        &self,
        request: TaskSummaryChangesParams,
        source: ChangeSource,
    ) -> Result<TaskSummaryChangesPage> {
        if !(1..=MAX_TASK_SUMMARY_CHANGE_EVENTS).contains(&request.limit) {
            return Err(Error::Invalid(format!(
                "limit must be 1 to {MAX_TASK_SUMMARY_CHANGE_EVENTS}"
            )));
        }
        // All reads use this connection; BEGIN pins the first read snapshot
        // through both invalidation selection and summary projection lookup.
        let tx = self.db.unchecked_transaction()?;
        let (page, summary_through_sequence) = self.read_task_changes(
            &TaskChangesParams {
                task_id: request.task_id,
                session_id: request.session_id,
                workspace_id: request.workspace_id,
                after: request.after,
                through: request.through,
                limit: request.limit,
            },
            source,
        )?;
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

    fn read_task_changes(
        &self,
        request: &TaskChangesParams,
        source: ChangeSource,
    ) -> Result<(TaskChangesPage, u64)> {
        let (events_sql, revisions_sql, owner) = match (
            &request.task_id,
            &request.session_id,
            &request.workspace_id,
        ) {
            (None, None, None) => (
                "SELECT sequence,task_id FROM events
                 WHERE ?1 IS NULL AND sequence>?2 AND sequence<=?3 ORDER BY sequence LIMIT ?4",
                "SELECT sequence,task_id FROM task_summary_revisions
                 WHERE ?1 IS NULL AND sequence>?2 AND sequence<=?3 ORDER BY sequence LIMIT ?4",
                None,
            ),
            (Some(id), None, None) => {
                self.require_task(id)?;
                (
                    "SELECT sequence,task_id FROM events
                     WHERE task_id=?1 AND sequence>?2 AND sequence<=?3 ORDER BY sequence LIMIT ?4",
                    "SELECT sequence,task_id FROM task_summary_revisions
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
                    "SELECT sequence,task_id FROM task_summary_revisions
                     WHERE session_id=?1 AND sequence>?2 AND sequence<=?3 ORDER BY sequence LIMIT ?4",
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
                    "SELECT sequence,task_id FROM task_summary_revisions
                     WHERE workspace_id=?1 AND sequence>?2 AND sequence<=?3 ORDER BY sequence LIMIT ?4",
                    Some(id.as_str()),
                )
            }
            _ => return Err(Error::Invalid("select one task change scope".into())),
        };
        replay::validate_event_limit(request.limit)?;
        let now = self.event_watermark()?;
        let through = request.through.unwrap_or(now);
        if request.after > through || through > now {
            return Err(Error::Invalid("invalid task change replay window".into()));
        }
        let cutover = if source != ChangeSource::Events {
            task_revisions::legacy_through(&self.db)?
        } else {
            through
        };
        let latest = if source == ChangeSource::LatestRevisions {
            Some(Supersession {
                later: self.db.prepare_cached(
                    "SELECT 1 FROM task_summary_revisions
                     WHERE task_id=?1 AND sequence>?2 AND sequence<=?3
                     UNION ALL
                     SELECT 1 FROM events WHERE task_id=?1 AND sequence>?2 AND sequence<=?4
                     LIMIT 1",
                )?,
                through: through as i64,
                legacy_through: through.min(cutover) as i64,
            })
        } else {
            None
        };
        let mut scan = ChangeScan {
            found: HashMap::new(),
            left: if latest.is_some() {
                MAX_TASK_CHANGE_EVENTS
            } else {
                request.limit
            },
            result_limit: request.limit as usize,
            last: request.after,
            latest,
        };
        // Ordered, disjoint ranges share one page budget, including the
        // extra row used to detect a continuation across the upgrade boundary.
        let has_more = scan.segment(
            &self.db,
            events_sql,
            owner,
            request.after,
            through.min(cutover),
        )? || scan.segment(
            &self.db,
            revisions_sql,
            owner,
            request.after.max(cutover),
            through,
        )?;
        let mut changes: Vec<_> = scan
            .found
            .into_iter()
            .map(|(task_id, sequence)| TaskChange { task_id, sequence })
            .collect();
        changes.sort_unstable_by_key(|change| change.sequence);
        let next_after = if has_more { scan.last } else { through };
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

#[derive(Clone, Copy, PartialEq, Eq)]
enum ChangeSource {
    Events,
    Revisions,
    LatestRevisions,
}

struct Supersession<'db> {
    later: rusqlite::CachedStatement<'db>,
    through: i64,
    legacy_through: i64,
}

struct ChangeScan<'db> {
    found: HashMap<String, u64>,
    left: u32,
    result_limit: usize,
    last: u64,
    latest: Option<Supersession<'db>>,
}

impl ChangeScan<'_> {
    fn segment(
        &mut self,
        db: &Connection,
        sql: &str,
        owner: Option<&str>,
        after: u64,
        through: u64,
    ) -> Result<bool> {
        if after >= through {
            return Ok(false);
        }
        let mut stmt = db.prepare_cached(sql)?;
        let mut rows = stmt.query(params![owner, after as i64, through as i64, self.left + 1])?;
        while let Some(row) = rows.next()? {
            if self.left == 0 || self.found.len() == self.result_limit {
                return Ok(true);
            }
            self.left -= 1;
            self.last = row.get::<_, i64>(0)? as u64;
            let id = row.get_ref(1)?.as_str().map_err(rusqlite::Error::from)?;
            // The two ordered sources share a fixed through bound. A later
            // revision outside this window will be eligible in the next one.
            if let Some(latest) = &mut self.latest
                && latest.later.exists(params![
                    id,
                    self.last as i64,
                    latest.through,
                    latest.legacy_through
                ])?
            {
                continue;
            }
            if let Some(sequence) = self.found.get_mut(id) {
                *sequence = self.last;
            } else {
                self.found.insert(id.to_owned(), self.last);
            }
        }
        Ok(false)
    }
}
