//! Ordered, durable event replay by task, session, or global cursor.
use super::*;

pub const MAX_EVENT_TARGET_BYTES: u32 = 8 * 1024 * 1024;

/// Index only the session and sequence so replay can seek directly to the
/// next page without copying event payloads or sorting a join across tasks.
pub(super) fn migrate_session_index(db: &mut Connection) -> Result<()> {
    let tx = db.transaction()?;
    tx.execute_batch(
        "CREATE TABLE IF NOT EXISTS session_event_index(
            session_id TEXT NOT NULL REFERENCES sessions(id),
            sequence INTEGER NOT NULL REFERENCES events(sequence),
            PRIMARY KEY(session_id,sequence)
        ) WITHOUT ROWID;
        INSERT INTO session_event_index(session_id,sequence)
        SELECT tasks.session_id,events.sequence FROM tasks
          JOIN events ON events.task_id=tasks.id WHERE true
        ON CONFLICT(session_id,sequence) DO NOTHING;
        CREATE TRIGGER IF NOT EXISTS session_event_index_insert AFTER INSERT ON events
        BEGIN
            INSERT INTO session_event_index(session_id,sequence)
            VALUES((SELECT session_id FROM tasks WHERE id=NEW.task_id),NEW.sequence);
        END;
        PRAGMA user_version=16;",
    )?;
    tx.commit()?;
    Ok(())
}

impl Kernel {
    pub fn event_watermark(&self) -> Result<u64> {
        Ok(self
            .db
            .query_row("SELECT COALESCE(MAX(sequence),0) FROM events", [], |row| {
                row.get::<_, i64>(0)
            })? as u64)
    }

    pub fn get_event(&self, sequence: u64) -> Result<Event> {
        if sequence == 0 {
            return Err(Error::Invalid("event sequence must be positive".into()));
        }
        let mut statement = self
            .db
            .prepare_cached("SELECT sequence,task_id,data FROM events WHERE sequence=?1")?;
        let mut rows = statement.query([validate_event_cursor(sequence)?])?;
        let row = rows
            .next()?
            .ok_or_else(|| Error::NotFound("event".into()))?;
        event_row(row)
    }

    pub fn list_events(&self, task_id: &str, after: u64, limit: u32) -> Result<EventsPage> {
        self.list_events_with_target(task_id, after, limit, None)
    }

    pub fn list_events_with_target(
        &self,
        task_id: &str,
        after: u64,
        limit: u32,
        target_bytes: Option<u32>,
    ) -> Result<EventsPage> {
        self.require_task(task_id)?;
        self.query_event_page(
            "SELECT sequence,task_id,data FROM events WHERE task_id=?1 AND sequence>?2 ORDER BY sequence LIMIT ?3",
            params![task_id, validate_event_cursor(after)?, validate_event_limit(limit)?],
            after,
            limit,
            target_bytes,
        )
    }

    pub fn list_all_events(&self, after: u64, limit: u32) -> Result<EventsPage> {
        self.list_all_events_with_target(after, limit, None)
    }

    pub fn list_all_events_with_target(
        &self,
        after: u64,
        limit: u32,
        target_bytes: Option<u32>,
    ) -> Result<EventsPage> {
        self.list_all_events_through(after, None, limit, target_bytes)
    }

    pub fn list_all_events_through(
        &self,
        after: u64,
        through: Option<u64>,
        limit: u32,
        target_bytes: Option<u32>,
    ) -> Result<EventsPage> {
        self.query_event_page(
            "SELECT sequence,task_id,data FROM events WHERE sequence>?1 AND sequence<=?2 ORDER BY sequence LIMIT ?3",
            params![validate_event_cursor(after)?, through.map(validate_event_cursor).transpose()?.unwrap_or(i64::MAX), validate_event_limit(limit)?],
            after,
            limit,
            target_bytes,
        )
    }

    pub fn list_session_events_through(
        &self,
        session_id: &str,
        after: u64,
        through: Option<u64>,
        limit: u32,
        target_bytes: Option<u32>,
    ) -> Result<EventsPage> {
        self.require_session(session_id)?;
        self.query_event_page(
            "SELECT events.sequence,events.task_id,events.data
             FROM session_event_index AS scoped JOIN events USING(sequence)
             WHERE scoped.session_id=?1 AND scoped.sequence>?2 AND scoped.sequence<=?3
             ORDER BY scoped.sequence LIMIT ?4",
            params![
                session_id,
                validate_event_cursor(after)?,
                through
                    .map(validate_event_cursor)
                    .transpose()?
                    .unwrap_or(i64::MAX),
                validate_event_limit(limit)?
            ],
            after,
            limit,
            target_bytes,
        )
    }

    fn query_event_page<P: rusqlite::Params>(
        &self,
        sql: &str,
        params: P,
        after: u64,
        limit: u32,
        target_bytes: Option<u32>,
    ) -> Result<EventsPage> {
        if target_bytes.is_some_and(|bytes| !(1024..=MAX_EVENT_TARGET_BYTES).contains(&bytes)) {
            return Err(Error::Invalid("targetBytes must be 1024..8388608".into()));
        }
        let mut stmt = self.db.prepare_cached(sql)?;
        let mut rows = stmt.query(params)?;
        let mut events = Vec::new();
        let mut next_after = after;
        let mut has_more = false;
        let mut bytes = 2; // Array brackets.
        while let Some(row) = rows.next()? {
            if events.len() == limit as usize {
                has_more = true;
                break;
            }
            let event = event_row(row)?;
            if let Some(target) = target_bytes {
                bytes += json_size(&event)? + usize::from(!events.is_empty());
                if bytes > target as usize && !events.is_empty() {
                    // This row remains the first candidate of the next page.
                    has_more = true;
                    break;
                }
            }
            next_after = event.sequence;
            events.push(event);
        }
        Ok(EventsPage {
            events,
            next_after,
            has_more,
        })
    }
}

fn event_row(row: &rusqlite::Row<'_>) -> Result<Event> {
    let data = row.get_ref(2)?.as_str().map_err(rusqlite::Error::from)?;
    Ok(Event {
        sequence: row.get::<_, i64>(0)? as u64,
        task_id: row.get(1)?,
        data: serde_json::from_str(data)?,
    })
}

fn validate_event_limit(limit: u32) -> Result<u32> {
    if !(1..=1000).contains(&limit) {
        return Err(Error::Invalid("limit must be 1..1000".into()));
    }
    Ok(limit + 1)
}
