//! Sparse index of events that seal an approval run's projection.
use super::*;
use rusqlite::Transaction;

// Both readers use the same indexed segment definition, including the first
// event of a root run and the approval decision that starts a continuation.
macro_rules! run_bounds_query {
    ($select:literal) => {
        concat!(
            "WITH bounds AS (
                SELECT r.task_id, a.sequence AS start_sequence, COALESCE(a.sequence, (
                    SELECT sequence FROM events e WHERE e.task_id=r.task_id ORDER BY sequence LIMIT 1
                )) AS first_sequence
                FROM task_requests r LEFT JOIN approval_runs a
                  ON a.session_id=r.session_id AND a.request_key=r.request_key
                WHERE r.session_id=?1 AND r.request_key=?2
             ), segment AS (
                SELECT b.*, (
                    SELECT sequence FROM task_run_ends e
                    WHERE e.task_id=b.task_id AND e.sequence>b.first_sequence ORDER BY sequence LIMIT 1
                ) AS last_sequence FROM bounds b
             ) ",
            $select
        )
    };
}

fn read_window(row: &rusqlite::Row<'_>) -> Result<ApprovalRunWindow> {
    let first: i64 = row
        .get::<_, Option<i64>>(1)?
        .ok_or_else(|| Error::NotFound("first event of task request".into()))?;
    Ok(ApprovalRunWindow {
        task_id: row.get(0)?,
        first_sequence: first as u64,
        last_sequence: row.get::<_, Option<i64>>(2)?.map(|value| value as u64),
    })
}

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
        let mut stmt = self.db.prepare_cached(run_bounds_query!(
            "SELECT task_id,first_sequence,last_sequence FROM segment"
        ))?;
        let mut rows = stmt.query(params![session, key])?;
        read_window(
            rows.next()?
                .ok_or_else(|| Error::NotFound("task request".into()))?,
        )
    }

    pub fn get_approval_run_replay(&self, session: &str, key: &str) -> Result<ApprovalRunReplay> {
        crate::task::validate_key(key)?;
        // Only the open segment needs the task's latest event. Both the
        // summary and the bounds are read by the same SQLite statement.
        let mut stmt = self.db.prepare_cached(run_bounds_query!(
            "SELECT b.task_id,b.first_sequence,b.last_sequence,b.start_sequence,s.data,
                CASE WHEN b.last_sequence IS NULL THEN (
                    SELECT MAX(sequence) FROM events e WHERE e.task_id=b.task_id
                ) END
              FROM segment b JOIN task_summaries s ON s.task_id=b.task_id"
        ))?;
        let mut rows = stmt.query(params![session, key])?;
        let row = rows
            .next()?
            .ok_or_else(|| Error::NotFound("task request".into()))?;
        let window = read_window(row)?;
        let through_sequence = match window.last_sequence {
            Some(last) => last,
            None => row.get::<_, i64>(5)? as u64,
        };
        let start_sequence = row.get::<_, Option<i64>>(3)?.map(|value| value as u64);
        let task = serde_json::from_str(row.get_ref(4)?.as_str().map_err(rusqlite::Error::from)?)?;
        Ok(ApprovalRunReplay {
            run: ApprovalRun {
                task,
                start_sequence,
            },
            window,
            through_sequence,
        })
    }
}
