//! Persist only commits that change the compact task summary.
use super::*;
use rusqlite::Transaction;

pub(super) fn migrate(db: &mut Connection) -> Result<()> {
    let tx = db.transaction()?;
    // Older event bodies cannot reconstruct the previous versions of a summary.
    // Keep a precise boundary and use conservative event invalidations before it.
    tx.execute_batch(
        "CREATE TABLE IF NOT EXISTS task_summary_revision_meta (
             id INTEGER PRIMARY KEY CHECK(id=1),
             legacy_through INTEGER NOT NULL
         );
         INSERT INTO task_summary_revision_meta(id,legacy_through)
             VALUES(1,(SELECT COALESCE(MAX(sequence),0) FROM events))
             ON CONFLICT(id) DO NOTHING;
         CREATE TABLE IF NOT EXISTS task_summary_revisions (
             sequence INTEGER PRIMARY KEY REFERENCES events(sequence),
             task_id TEXT NOT NULL REFERENCES task_summaries(task_id),
             session_id TEXT NOT NULL,
             workspace_id TEXT NOT NULL
         );
         CREATE INDEX IF NOT EXISTS task_revisions_task
             ON task_summary_revisions(task_id,sequence);
         CREATE INDEX IF NOT EXISTS task_revisions_session
             ON task_summary_revisions(session_id,sequence,task_id);
         CREATE INDEX IF NOT EXISTS task_revisions_workspace
             ON task_summary_revisions(workspace_id,sequence,task_id);
         PRAGMA user_version=45;",
    )?;
    tx.commit()?;
    Ok(())
}

pub(super) fn record(tx: &Transaction<'_>, task: &Task, sequence: i64) -> Result<()> {
    tx.prepare_cached(
        "INSERT INTO task_summary_revisions(sequence,task_id,session_id,workspace_id)
         VALUES(?1,?2,?3,(SELECT workspace_id FROM sessions WHERE id=?3))",
    )?
    .execute(params![sequence, task.id, task.session_id])?;
    Ok(())
}

pub(super) fn legacy_through(db: &Connection) -> Result<u64> {
    Ok(db
        .prepare_cached("SELECT legacy_through FROM task_summary_revision_meta WHERE id=1")?
        .query_row([], |row| row.get::<_, i64>(0))? as u64)
}
