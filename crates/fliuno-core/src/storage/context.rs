//! Indexed history projections commit atomically with TaskCompleted. The query
//! checks byte lengths in SQLite before loading any potentially large record.
use super::*;
use rusqlite::Transaction;

pub(super) fn migrate(db: &mut Connection) -> Result<()> {
    let tx = db.transaction()?;
    tx.execute_batch(
        "CREATE TABLE IF NOT EXISTS session_turns(
        task_id TEXT PRIMARY KEY REFERENCES tasks(id),
        session_id TEXT NOT NULL REFERENCES sessions(id),
        sequence INTEGER NOT NULL UNIQUE REFERENCES events(sequence),
        data TEXT NOT NULL);
        CREATE INDEX IF NOT EXISTS session_turns_cursor ON session_turns(session_id, sequence);",
    )?;
    {
        // Stream old tasks one at a time. Never rewrite their frozen inputs or
        // event log while adding the derived history index.
        let mut statement = tx.prepare(
            "SELECT tasks.data, MIN(events.sequence) FROM tasks
            JOIN events ON events.task_id=tasks.id
            WHERE json_extract(events.data,'$.type')='task_completed'
            AND tasks.id NOT IN (SELECT task_id FROM session_turns)
            GROUP BY tasks.id ORDER BY MIN(events.sequence)",
        )?;
        let mut rows = statement.query([])?;
        while let Some(row) = rows.next()? {
            let task: Task = serde_json::from_str(&row.get::<_, String>(0)?)?;
            record(&tx, &task, row.get::<_, i64>(1)? as u64)?;
        }
    }
    tx.pragma_update(None, "user_version", 5)?;
    tx.commit()?;
    Ok(())
}

pub(super) fn record(tx: &Transaction<'_>, task: &Task, sequence: u64) -> Result<()> {
    let mut statement = tx.prepare("SELECT id, json_extract(data,'$.title'), json_extract(data,'$.mediaType') FROM artifacts WHERE task_id=?1 ORDER BY rowid")?;
    let artifacts = statement
        .query_map([&task.id], |row| {
            Ok(ContextArtifact {
                id: row.get(0)?,
                title: row.get(1)?,
                media_type: row.get(2)?,
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let Some(turn) = crate::context::project_turn(task, sequence, artifacts) else {
        return Ok(());
    };
    tx.execute(
        "INSERT INTO session_turns(task_id,session_id,sequence,data) VALUES(?1,?2,?3,?4)",
        params![
            task.id,
            task.session_id,
            validate_event_cursor(sequence)?,
            serde_json::to_string(&turn)?
        ],
    )?;
    Ok(())
}

impl Kernel {
    /// Read only selected projections. A foreign, unfinished or absent ID has
    /// the same diagnostic; no Task snapshot or content is loaded to explain it.
    pub(crate) fn selected_context(
        &self,
        session: &str,
        through: u64,
        options: &ContextOptions,
    ) -> Result<(u64, Vec<ContextTurn>)> {
        let through = validate_event_cursor(through)?;
        let total: i64 = self.db.query_row(
            "SELECT COUNT(*) FROM session_turns WHERE session_id=?1 AND sequence<=?2",
            params![session, through],
            |row| row.get(0),
        )?;
        let ids = options
            .task_ids
            .as_ref()
            .ok_or_else(|| Error::Invalid("selected context requires taskIds".into()))?;
        let mut statement = self.db.prepare(
            "SELECT CASE WHEN length(CAST(data AS BLOB))<=?4 THEN data END FROM session_turns
            WHERE session_id=?1 AND task_id=?2 AND sequence<=?3",
        )?;
        let mut turns: Vec<ContextTurn> = Vec::new();
        let mut bytes = 0;
        for id in ids {
            let record: Option<Option<String>> = statement
                .query_row(params![session, id, through, options.max_bytes], |row| {
                    row.get(0)
                })
                .optional()?;
            let data = match record {
                None => {
                    return Err(Error::Invalid(
                        "selected context task is not a completed record in this session".into(),
                    ));
                }
                Some(None) => {
                    return Err(Error::Invalid(
                        "selected context tasks exceed maxBytes".into(),
                    ));
                }
                Some(Some(data)) => data,
            };
            bytes += data.len();
            if bytes > options.max_bytes as usize {
                return Err(Error::Invalid(
                    "selected context tasks exceed maxBytes".into(),
                ));
            }
            turns.push(serde_json::from_str(&data)?);
        }
        turns.sort_unstable_by_key(|turn| turn.completion_sequence);
        Ok((total as u64, turns))
    }

    pub(crate) fn context_watermark(&self) -> Result<u64> {
        Ok(self
            .db
            .query_row("SELECT COALESCE(MAX(sequence),0) FROM events", [], |row| {
                row.get::<_, i64>(0)
            })? as u64)
    }

    pub(crate) fn context_candidates(
        &self,
        session: &str,
        through: u64,
        options: &ContextOptions,
    ) -> Result<(u64, Vec<Option<ContextTurn>>)> {
        let through = validate_event_cursor(through)?;
        let total: i64 = self.db.query_row(
            "SELECT COUNT(*) FROM session_turns WHERE session_id=?1 AND sequence<=?2",
            params![session, through],
            |row| row.get(0),
        )?;
        let mut statement = self.db.prepare(
            "SELECT CASE WHEN length(CAST(data AS BLOB))<=?3 THEN data END FROM session_turns
            WHERE session_id=?1 AND sequence<=?2 ORDER BY sequence DESC LIMIT ?4",
        )?;
        let rows = statement.query_map(
            params![session, through, options.max_bytes, options.max_turns],
            |row| row.get::<_, Option<String>>(0),
        )?;
        let mut turns = Vec::new();
        for row in rows {
            let Some(data) = row? else {
                turns.push(None);
                break;
            };
            turns.push(Some(serde_json::from_str(&data)?));
        }
        Ok((total as u64, turns))
    }
}
