//! Look up task counters without reading inbox rows or task bodies.
use super::*;

impl Kernel {
    pub fn work_session_counts(
        &self,
        request: &WorkSessionCountsParams,
    ) -> Result<WorkSessionCounts> {
        let ids = &request.session_ids;
        if !(1..=MAX_WORK_SESSION_COUNT_BATCH).contains(&ids.len()) {
            return Err(Error::Invalid(format!(
                "sessionIds must contain 1 to {MAX_WORK_SESSION_COUNT_BATCH} entries"
            )));
        }
        if std::iter::once(&request.workspace_id).chain(ids).any(|id| {
            !(1..=MAX_WORK_SESSION_COUNT_ID_BYTES).contains(&id.len())
                || !id.bytes().all(|byte| byte.is_ascii_graphic())
        }) {
            return Err(Error::Invalid("invalid workspace or session ID".into()));
        }

        let tx = self.db.unchecked_transaction()?;
        self.require_workspace(&request.workspace_id)?;
        let through_sequence = self.event_watermark()?;
        let mut items = vec![None; ids.len()];
        {
            // Join ownership first so a session from another workspace does
            // not load any counters. A session with no tasks has no count row.
            let mut select = tx.prepare_cached(
                "SELECT requested.key, session.id,
                        COALESCE(c.running,0), COALESCE(c.waiting_approval,0),
                        COALESCE(c.interrupted,0), COALESCE(c.completed,0),
                        COALESCE(c.cancelled,0), COALESCE(c.failed,0)
                 FROM json_each(?1) AS requested
                 LEFT JOIN sessions AS session
                    ON session.id=requested.value AND session.workspace_id=?2
                 LEFT JOIN work_status_counts AS c ON c.scope=1 AND c.owner=session.id",
            )?;
            let encoded_ids = serde_json::to_string(ids)?;
            let mut rows = select.query(params![encoded_ids, request.workspace_id])?;
            while let Some(row) = rows.next()? {
                let index = row.get::<_, i64>(0)? as usize;
                if let Some(session_id) = row.get::<_, Option<String>>(1)? {
                    items[index] = Some(WorkSessionCount {
                        session_id,
                        task_counts: read_counts(row, 2)?,
                    });
                }
            }
        }
        tx.commit()?;
        Ok(WorkSessionCounts {
            items,
            through_sequence,
        })
    }
}
