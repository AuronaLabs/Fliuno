//! Sparse index of events that seal an approval run's projection.
use super::*;
use rusqlite::Transaction;

pub(super) fn migrate(db: &mut Connection) -> Result<()> {
    let tx = db.transaction()?;
    tx.execute_batch(
        "CREATE TABLE IF NOT EXISTS task_run_ends(
            task_id TEXT NOT NULL REFERENCES tasks(id),
            sequence INTEGER NOT NULL REFERENCES events(sequence),
            PRIMARY KEY(task_id,sequence)
        ) WITHOUT ROWID;
        INSERT INTO task_run_ends(task_id,sequence)
            SELECT task_id,sequence FROM events WHERE json_extract(data,'$.type') IN
              ('approval_requested','task_interrupted','task_cancelled','task_failed','task_completed')
            ON CONFLICT(task_id,sequence) DO NOTHING;
        PRAGMA user_version=39;",
    )?;
    tx.commit()?;
    Ok(())
}

pub(super) fn record(
    tx: &Transaction<'_>,
    task_id: &str,
    event: &EventKind,
    sequence: i64,
) -> Result<()> {
    if matches!(
        event,
        EventKind::ApprovalRequested { .. }
            | EventKind::TaskInterrupted
            | EventKind::TaskCancelled
            | EventKind::TaskFailed { .. }
            | EventKind::TaskCompleted { .. }
    ) {
        tx.prepare_cached("INSERT INTO task_run_ends(task_id,sequence) VALUES(?1,?2)")?
            .execute(params![task_id, sequence])?;
    }
    Ok(())
}

impl Kernel {
    /// Resolve the inclusive durable bounds of one run, independent of current task state.
    pub fn get_approval_run_window(&self, session: &str, key: &str) -> Result<ApprovalRunWindow> {
        crate::task::validate_key(key)?;
        let (task_id, first, last): (String, Option<i64>, Option<i64>) = self.db.prepare_cached(
            "WITH binding AS (
                SELECT r.task_id, COALESCE(a.sequence, (
                    SELECT sequence FROM events e WHERE e.task_id=r.task_id ORDER BY sequence LIMIT 1
                )) AS first_sequence
                FROM task_requests r LEFT JOIN approval_runs a
                  ON a.session_id=r.session_id AND a.request_key=r.request_key
                WHERE r.session_id=?1 AND r.request_key=?2
             )
             SELECT b.task_id,b.first_sequence,(
                 SELECT sequence FROM task_run_ends e
                 WHERE e.task_id=b.task_id AND e.sequence>b.first_sequence ORDER BY sequence LIMIT 1
             ) FROM binding b",
        )?.query_row(params![session,key], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?)))
            .optional()?.ok_or_else(|| Error::NotFound("task request".into()))?;
        let first = first.ok_or_else(|| Error::NotFound("first event of task request".into()))?;
        Ok(ApprovalRunWindow {
            task_id,
            first_sequence: first as u64,
            last_sequence: last.map(|value| value as u64),
        })
    }
}
