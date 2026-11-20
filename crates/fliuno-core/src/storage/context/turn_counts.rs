//! Keep exact counts at historical cursors with a bounded suffix scan.
use super::*;

const CHECKPOINT_INTERVAL: u32 = 128;

/// History is append-only. Keep one small row per session so ordinary reads
/// can report the exact omitted count without scanning completed turns.
pub(in crate::storage) fn migrate_turn_stats(db: &mut Connection) -> Result<()> {
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

pub(in crate::storage) fn migrate_checkpoints(db: &mut Connection) -> Result<()> {
    let tx = db.transaction()?;
    // Backfill from the covering history index. No task or history JSON is read.
    tx.execute_batch(&format!(
        "CREATE TABLE IF NOT EXISTS session_turn_checkpoints(
            session_id TEXT NOT NULL REFERENCES sessions(id),
            sequence INTEGER NOT NULL REFERENCES session_turns(sequence) ON DELETE CASCADE,
            turn_count INTEGER NOT NULL,
            PRIMARY KEY(session_id,sequence)
        ) WITHOUT ROWID;
        INSERT INTO session_turn_checkpoints(session_id,sequence,turn_count)
        SELECT session_id, sequence, ordinal FROM (
            SELECT session_id, sequence,
                   ROW_NUMBER() OVER (PARTITION BY session_id ORDER BY sequence) AS ordinal
            FROM session_turns
        ) WHERE ordinal % {CHECKPOINT_INTERVAL}=0
        ON CONFLICT(session_id,sequence) DO UPDATE SET turn_count=excluded.turn_count;
        DROP TRIGGER session_turn_stats_insert;
        CREATE TRIGGER session_turn_stats_insert AFTER INSERT ON session_turns
        BEGIN
            INSERT INTO session_turn_stats(session_id,turn_count,latest_sequence)
            VALUES(NEW.session_id,1,NEW.sequence)
            ON CONFLICT(session_id) DO UPDATE SET
                turn_count=turn_count+1, latest_sequence=MAX(latest_sequence,excluded.latest_sequence);
            INSERT INTO session_turn_checkpoints(session_id,sequence,turn_count)
            SELECT NEW.session_id, NEW.sequence, turn_count FROM session_turn_stats
            WHERE session_id=NEW.session_id AND turn_count % {CHECKPOINT_INTERVAL}=0;
        END;
        PRAGMA user_version=51;"
    ))?;
    tx.commit()?;
    Ok(())
}

impl Kernel {
    pub(crate) fn context_turn_count(&self, session: &str, through: u64) -> Result<u64> {
        let mut statement = self.db.prepare_cached(
            "SELECT COALESCE((SELECT CASE WHEN latest_sequence<=?2 THEN turn_count ELSE
                COALESCE((SELECT c.turn_count + (
                    SELECT COUNT(*) FROM session_turns
                    WHERE session_id=?1 AND sequence>c.sequence AND sequence<=?2
                ) FROM session_turn_checkpoints c
                WHERE c.session_id=?1 AND c.sequence<=?2 ORDER BY c.sequence DESC LIMIT 1),
                (SELECT COUNT(*) FROM session_turns WHERE session_id=?1 AND sequence<=?2))
             END FROM session_turn_stats WHERE session_id=?1), 0)",
        )?;
        // With append-only history, the tail (or the prefix before the first
        // checkpoint) contains at most CHECKPOINT_INTERVAL-1 matching turns.
        let count: i64 = statement
            .query_row(params![session, validate_event_cursor(through)?], |row| {
                row.get(0)
            })?;
        Ok(count as u64)
    }
}
