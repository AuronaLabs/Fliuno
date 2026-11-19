//! Seek directly to model usage reports in the committed event log.
use super::*;
use rusqlite::Transaction;

pub(super) fn migrate(db: &mut Connection) -> Result<()> {
    let tx = db.transaction()?;
    tx.execute_batch(
        "CREATE TABLE IF NOT EXISTS model_usage_index(
            task_id TEXT NOT NULL REFERENCES tasks(id),
            sequence INTEGER NOT NULL REFERENCES events(sequence),
            PRIMARY KEY(task_id,sequence)
        ) WITHOUT ROWID;
        INSERT INTO model_usage_index(task_id,sequence)
        SELECT task_id,sequence FROM events WHERE json_extract(data,'$.type')='model_usage_reported'
        ON CONFLICT(task_id,sequence) DO NOTHING;
        PRAGMA user_version=37;",
    )?;
    tx.commit()?;
    Ok(())
}

pub(super) fn record(tx: &Transaction<'_>, task_id: &str, sequence: i64) -> Result<()> {
    tx.prepare_cached("INSERT INTO model_usage_index(task_id,sequence) VALUES(?1,?2)")?
        .execute(params![task_id, sequence])?;
    Ok(())
}

impl Kernel {
    pub fn list_model_usage_events(&self, request: &EventsListParams) -> Result<EventsPage> {
        self.require_task(&request.task_id)?;
        let after = validate_event_cursor(request.after)?;
        let through = request
            .through
            .map(validate_event_cursor)
            .transpose()?
            .unwrap_or(i64::MAX);
        let fetch_limit = replay::validate_event_limit(request.limit)?;
        self.query_event_page(
            "SELECT events.sequence,events.task_id,events.data
             FROM model_usage_index AS usage JOIN events USING(sequence)
             WHERE usage.task_id=?1 AND usage.sequence>?2 AND usage.sequence<=?3
             ORDER BY usage.sequence LIMIT ?4",
            params![request.task_id, after, through, fetch_limit],
            request.after,
            request.limit,
            request.target_bytes,
        )
    }
}
