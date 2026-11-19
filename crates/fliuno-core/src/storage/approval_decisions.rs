//! Index committed approval decisions without duplicating event data.
use super::*;
use rusqlite::Transaction;

pub(super) fn migrate(db: &mut Connection) -> Result<()> {
    let tx = db.transaction()?;
    tx.execute_batch(
        "CREATE TABLE IF NOT EXISTS approval_decisions(
            task_id TEXT NOT NULL REFERENCES tasks(id),
            approval_id TEXT NOT NULL,
            sequence INTEGER NOT NULL REFERENCES events(sequence),
            PRIMARY KEY(task_id,approval_id)
        ) WITHOUT ROWID;
        INSERT INTO approval_decisions(task_id,approval_id,sequence)
        SELECT task_id,json_extract(data,'$.approvalId'),sequence FROM events
        WHERE json_extract(data,'$.type') IN ('approval_resolved','agent_approval_resolved')
        ON CONFLICT(task_id,approval_id) DO NOTHING;
        PRAGMA user_version=34;",
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
    if let EventKind::ApprovalResolved { approval_id, .. }
    | EventKind::AgentApprovalResolved { approval_id, .. } = event
    {
        tx.prepare_cached(
            "INSERT INTO approval_decisions(task_id,approval_id,sequence) VALUES(?1,?2,?3)",
        )?
        .execute(params![task_id, approval_id, sequence])?;
    }
    Ok(())
}

impl Kernel {
    pub fn get_approval_decision(&self, task_id: &str, approval_id: &str) -> Result<Event> {
        if approval_id.is_empty() || approval_id.len() > 128 {
            return Err(Error::Invalid("invalid approval ID".into()));
        }
        let mut statement = self.db.prepare_cached(
            "SELECT events.sequence,events.task_id,events.data
             FROM approval_decisions JOIN events USING(sequence)
             WHERE approval_decisions.task_id=?1 AND approval_id=?2",
        )?;
        let mut rows = statement.query(params![task_id, approval_id])?;
        let row = rows
            .next()?
            .ok_or_else(|| Error::NotFound("approval decision".into()))?;
        replay::event_row(row)
    }
}
