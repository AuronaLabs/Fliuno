//! Indexed history projections commit atomically with TaskCompleted. The query
//! checks byte lengths in SQLite before loading any potentially large record.
use super::*;
use rusqlite::Transaction;

mod conversation;
mod history_list;
mod search;
pub(super) use conversation::migrate as migrate_conversations;

fn projection(detail: ContextDetail, budget: &str, extra_columns: &str) -> String {
    // These SQL fragments are internal constants. Use octet_length on the
    // column itself so rejected records do not require reading overflow pages.
    match detail {
        ContextDetail::Full => format!(
            "SELECT CASE WHEN octet_length(s.data)<={budget} THEN s.data END{extra_columns} FROM session_turns s"
        ),
        ContextDetail::Conversation => {
            conversation_projection(budget, extra_columns, "session_turns s")
        }
    }
}

fn conversation_projection(budget: &str, extra_columns: &str, source: &str) -> String {
    format!(
        "SELECT CASE WHEN c.task_id IS NOT NULL
            THEN CASE WHEN octet_length(c.data)<={budget} THEN c.data END
            ELSE CASE WHEN octet_length(s.data)<={budget} THEN s.data END END{extra_columns}
         FROM {source} LEFT JOIN conversation_turns c ON c.task_id=s.task_id"
    )
}

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
            record(&tx, &task, row.get::<_, i64>(1)? as u64, false)?;
        }
    }
    tx.pragma_update(None, "user_version", 5)?;
    tx.commit()?;
    Ok(())
}

/// History is append-only. Keep one small row per session so ordinary reads
/// can report the exact omitted count without scanning completed turns.
pub(super) fn migrate_turn_stats(db: &mut Connection) -> Result<()> {
    let tx = db.transaction()?;
    tx.execute_batch(
        "CREATE TABLE IF NOT EXISTS session_turn_stats(
            session_id TEXT PRIMARY KEY REFERENCES sessions(id),
            turn_count INTEGER NOT NULL,
            latest_sequence INTEGER NOT NULL
        ) WITHOUT ROWID;
        INSERT INTO session_turn_stats(session_id,turn_count,latest_sequence)
        SELECT session_id, COUNT(*), MAX(sequence) FROM session_turns GROUP BY session_id
        ON CONFLICT(session_id) DO UPDATE SET
            turn_count=excluded.turn_count, latest_sequence=excluded.latest_sequence;
        CREATE TRIGGER IF NOT EXISTS session_turn_stats_insert AFTER INSERT ON session_turns
        BEGIN
            INSERT INTO session_turn_stats(session_id,turn_count,latest_sequence)
            VALUES(NEW.session_id,1,NEW.sequence)
            ON CONFLICT(session_id) DO UPDATE SET
                turn_count=turn_count+1, latest_sequence=MAX(latest_sequence,excluded.latest_sequence);
        END;
        PRAGMA user_version=14;",
    )?;
    tx.commit()?;
    Ok(())
}

pub(super) fn record(
    tx: &Transaction<'_>,
    task: &Task,
    sequence: u64,
    index_conversation: bool,
) -> Result<()> {
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
    let Some(mut turn) = crate::context::project_turn(task, sequence, artifacts) else {
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
    if index_conversation {
        conversation::record(tx, &mut turn)?;
    }
    Ok(())
}

/// Index completion order without copying full history or scanning other workspaces.
pub(super) fn migrate_workspace_turns(db: &mut Connection) -> Result<()> {
    let tx = db.transaction()?;
    tx.execute_batch(
        "CREATE TABLE IF NOT EXISTS workspace_turn_index(
            workspace_id TEXT NOT NULL REFERENCES workspaces(id),
            sequence INTEGER NOT NULL REFERENCES session_turns(sequence) ON DELETE CASCADE,
            PRIMARY KEY(workspace_id,sequence)
        ) WITHOUT ROWID;
        INSERT INTO workspace_turn_index(workspace_id,sequence)
        SELECT s.workspace_id,t.sequence FROM session_turns t
        JOIN sessions s ON s.id=t.session_id WHERE true
        ON CONFLICT(workspace_id,sequence) DO NOTHING;
        CREATE TRIGGER IF NOT EXISTS workspace_turn_index_insert AFTER INSERT ON session_turns
        BEGIN
            INSERT INTO workspace_turn_index(workspace_id,sequence)
            SELECT workspace_id,NEW.sequence FROM sessions WHERE id=NEW.session_id;
        END;
        PRAGMA user_version=33;",
    )?;
    tx.commit()?;
    Ok(())
}

impl Kernel {
    /// Only matched conversation records may load the full tool history.
    pub(crate) fn full_context_candidate(
        &self,
        session: &str,
        sequence: u64,
        max_bytes: u32,
    ) -> Result<Option<ContextTurn>> {
        let data: Option<String> = self
            .db
            .prepare_cached(
                "SELECT CASE WHEN octet_length(data)<=?3 THEN data END
             FROM session_turns WHERE session_id=?1 AND sequence=?2",
            )?
            .query_row(
                params![session, validate_event_cursor(sequence)?, max_bytes],
                |row| row.get(0),
            )?;
        Ok(data.as_deref().map(serde_json::from_str).transpose()?)
    }

    /// Read only selected projections. A foreign, unfinished or absent ID has
    /// the same diagnostic; no Task snapshot or content is loaded to explain it.
    pub(crate) fn selected_context(
        &self,
        session: &str,
        through: u64,
        options: &ContextOptions,
    ) -> Result<(u64, Vec<ContextTurn>)> {
        let total = self.context_turn_count(session, through)?;
        let through = validate_event_cursor(through)?;
        let ids = options
            .task_ids
            .as_ref()
            .ok_or_else(|| Error::Invalid("selected context requires taskIds".into()))?;
        let sql = format!(
            "{} WHERE s.session_id=?1 AND s.task_id=?2 AND s.sequence<=?3",
            projection(options.detail, "?4", "")
        );
        let mut statement = self.db.prepare_cached(&sql)?;
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
        Ok((total, turns))
    }

    pub(crate) fn context_turn_count(&self, session: &str, through: u64) -> Result<u64> {
        let count: i64 = self.db.query_row(
            // CASE avoids visiting the history index unless the caller needs
            // a count at an older snapshot cursor.
            "SELECT COALESCE((SELECT CASE WHEN latest_sequence<=?2 THEN turn_count
                ELSE (SELECT COUNT(*) FROM session_turns WHERE session_id=?1 AND sequence<=?2)
                END FROM session_turn_stats WHERE session_id=?1), 0)",
            params![session, validate_event_cursor(through)?],
            |row| row.get(0),
        )?;
        Ok(count as u64)
    }

    pub(crate) fn visit_context_candidates(
        &self,
        session: &str,
        through: u64,
        options: &ContextOptions,
        mut visit: impl FnMut(Option<ContextTurn>) -> Result<bool>,
    ) -> Result<()> {
        let through = validate_event_cursor(through)?;
        let candidates = match options.selection {
            ContextSelection::Recent => options.max_turns,
            ContextSelection::FitRecent => crate::MAX_CONTEXT_CANDIDATES,
            ContextSelection::MatchRecent => {
                return Err(Error::Invalid(
                    "history matching requires the conversation scan".into(),
                ));
            }
        };
        let sql = format!(
            "{} WHERE s.session_id=?1 AND s.sequence<=?2 ORDER BY s.sequence DESC LIMIT ?4",
            projection(options.detail, "?3", "")
        );
        let mut statement = self.db.prepare_cached(&sql)?;
        let rows = statement.query_map(
            params![session, through, options.max_bytes, candidates],
            |row| row.get::<_, Option<String>>(0),
        )?;
        for row in rows {
            let turn = row?.as_deref().map(serde_json::from_str).transpose()?;
            if !visit(turn)? {
                break;
            }
        }
        Ok(())
    }
}
