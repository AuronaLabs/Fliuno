//! Bind a continuation identity to the same transaction as its approval event.
use super::*;
use crate::ApprovalOutcome;
use rusqlite::Transaction;

pub(super) fn migrate(db: &mut Connection) -> Result<()> {
    let tx = db.transaction()?;
    tx.execute_batch(
        "CREATE TABLE IF NOT EXISTS approval_runs(
            session_id TEXT NOT NULL,
            request_key TEXT NOT NULL,
            parent_key TEXT NOT NULL,
            sequence INTEGER NOT NULL UNIQUE REFERENCES events(sequence),
            PRIMARY KEY(session_id,request_key),
            FOREIGN KEY(session_id,request_key) REFERENCES task_requests(session_id,request_key),
            FOREIGN KEY(session_id,parent_key) REFERENCES task_requests(session_id,request_key)
        ) WITHOUT ROWID;
        PRAGMA user_version=38;",
    )?;
    tx.commit()?;
    Ok(())
}

pub(super) fn record(
    tx: &Transaction<'_>,
    task: &str,
    p: &ApprovalResumeParams,
    sequence: i64,
) -> Result<()> {
    tx.prepare_cached("INSERT INTO task_requests VALUES(?1,?2,?3)")?
        .execute(params![p.session_id, p.client_request_id, task])?;
    tx.prepare_cached("INSERT INTO approval_runs VALUES(?1,?2,?3,?4)")?
        .execute(params![
            p.session_id,
            p.client_request_id,
            p.parent_request_id,
            sequence
        ])?;
    Ok(())
}

impl Kernel {
    /// Only return the still-pending approval that sealed this run.
    pub fn get_approval_run_pending(
        &self,
        p: &ApprovalRunPendingParams,
    ) -> Result<ApprovalDetails> {
        let window = self.get_approval_run_window(&p.session_id, &p.client_request_id)?;
        let missing = || Error::NotFound("pending approval for this run".into());
        let boundary = window.last_sequence.ok_or_else(missing)?;
        let matching: bool = self.db.prepare_cached(
            "SELECT EXISTS(SELECT 1 FROM events WHERE sequence=?1 AND task_id=?2
               AND json_extract(data,'$.type')='approval_requested' AND json_extract(data,'$.approval.id')=?3)",
        )?.query_row(params![validate_event_cursor(boundary)?, window.task_id, p.approval_id], |row| row.get(0))?;
        if !matching {
            return Err(missing());
        }
        self.get_pending_approval(&window.task_id, &p.approval_id)
    }

    pub fn get_approval_run(&self, session: &str, key: &str) -> Result<ApprovalRun> {
        crate::task::validate_key(key)?;
        let mut statement = self.db.prepare_cached(
            "SELECT s.data,a.sequence FROM task_requests r
             JOIN task_summaries s ON s.task_id=r.task_id
             LEFT JOIN approval_runs a ON a.session_id=r.session_id AND a.request_key=r.request_key
             WHERE r.session_id=?1 AND r.request_key=?2",
        )?;
        let mut rows = statement.query(params![session, key])?;
        let row = rows
            .next()?
            .ok_or_else(|| Error::NotFound("task request".into()))?;
        let data = row.get_ref(0)?.as_str().map_err(rusqlite::Error::from)?;
        let task = serde_json::from_str(data)?;
        let start_sequence = row
            .get::<_, Option<i64>>(1)?
            .map(|sequence| sequence as u64);
        Ok(ApprovalRun {
            task,
            start_sequence,
        })
    }

    pub(crate) fn has_approval_binding(&self, session: &str, key: &str) -> Result<bool> {
        Ok(self
            .db
            .prepare_cached(
                "SELECT EXISTS(SELECT 1 FROM approval_runs WHERE session_id=?1 AND request_key=?2)",
            )?
            .query_row(params![session, key], |row| row.get(0))?)
    }

    pub fn resume_approval_run(
        &mut self,
        p: &ApprovalResumeParams,
    ) -> Result<Option<ApprovalOutcome>> {
        crate::task::validate_key(&p.client_request_id)?;
        crate::task::validate_key(&p.parent_request_id)?;
        if p.approval_id.is_empty()
            || p.approval_id.len() > 128
            || p.approval_id.chars().any(char::is_control)
        {
            return Err(Error::Invalid("invalid approval ID".into()));
        }
        match self.get_approval_run(&p.session_id, &p.client_request_id) {
            Ok(run) => {
                let Some(sequence) = run.start_sequence else {
                    return Err(Error::Conflict(
                        "request ID already belongs to a task start".into(),
                    ));
                };
                let parent: String = self.db.prepare_cached(
                    "SELECT parent_key FROM approval_runs WHERE session_id=?1 AND request_key=?2",
                )?.query_row(params![p.session_id, p.client_request_id], |row| row.get(0))?;
                let event = self.get_event(sequence)?;
                if parent != p.parent_request_id
                    || event.task_id != run.task.id
                    || !matches!(event.data, EventKind::ApprovalResolved { approval_id, decision }
                        if approval_id == p.approval_id && decision == p.decision)
                {
                    return Err(Error::Conflict(
                        "request ID already has a different approval response".into(),
                    ));
                }
                return Ok(None);
            }
            Err(Error::NotFound(_)) => {}
            Err(error) => return Err(error),
        }
        let parent = self.get_approval_run(&p.session_id, &p.parent_request_id)?;
        if parent.task.backend != TaskBackend::Model {
            return Err(Error::Conflict(
                "only model tool approvals can resume a run".into(),
            ));
        }
        self.get_pending_approval(&parent.task.id, &p.approval_id)?;
        let start = validate_event_cursor(parent.start_sequence.unwrap_or(0))?;
        // Only the first pause in this segment is answerable with its run ID.
        // A model step resumed through a different API cannot be claimed here.
        let first: Option<String> = self
            .db
            .prepare_cached(
                "SELECT json_extract(data,'$.approval.id') FROM events WHERE sequence=(
                SELECT sequence FROM task_run_ends WHERE task_id=?1 AND sequence>?2
                ORDER BY sequence LIMIT 1
             )",
            )?
            .query_row(params![parent.task.id, start], |row| row.get(0))
            .optional()?
            .flatten();
        if first.as_deref() != Some(p.approval_id.as_str()) {
            return Err(Error::Conflict(
                "approval did not interrupt this run".into(),
            ));
        }
        let task = self.get_task(&parent.task.id)?;
        self.prepare_model_approval(task, &p.approval_id, p.decision, Some(p))
            .map(Some)
    }

    /// Cancel only through the current run identity, before releasing the kernel lock.
    pub fn cancel_approval_run(&mut self, session: &str, key: &str) -> Result<TaskSummary> {
        let run = self.get_approval_run(session, key)?;
        let start = validate_event_cursor(run.start_sequence.unwrap_or(0))?;
        let advanced: bool = self
            .db
            .prepare_cached(
                "SELECT EXISTS(SELECT 1 FROM events e LEFT JOIN approval_runs a USING(sequence)
               WHERE e.task_id=?1 AND e.sequence>?2
               AND (a.sequence IS NOT NULL OR json_extract(e.data,'$.type')='task_resumed'))",
            )?
            .query_row(params![run.task.id, start], |row| row.get(0))?;
        if advanced {
            return Err(Error::Conflict("a newer run owns task control".into()));
        }
        self.cancel_task(&run.task.id)?;
        self.get_task_summary(&run.task.id)
    }
}
