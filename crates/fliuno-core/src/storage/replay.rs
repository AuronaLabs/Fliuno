//! Ordered, durable event replay for both task-scoped and global cursors.
use super::*;

pub const MAX_EVENT_TARGET_BYTES: u32 = 8 * 1024 * 1024;

impl Kernel {
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
        self.query_event_page(
            "SELECT sequence,task_id,data FROM events WHERE sequence>?1 ORDER BY sequence LIMIT ?2",
            params![validate_event_cursor(after)?, validate_event_limit(limit)?],
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
            let sequence = row.get::<_, i64>(0)? as u64;
            let event = Event {
                sequence,
                task_id: row.get(1)?,
                data: serde_json::from_str(
                    row.get_ref(2)?.as_str().map_err(rusqlite::Error::from)?,
                )?,
            };
            if let Some(target) = target_bytes {
                bytes += json_size(&event)? + usize::from(!events.is_empty());
                if bytes > target as usize && !events.is_empty() {
                    // This row remains the first candidate of the next page.
                    has_more = true;
                    break;
                }
            }
            events.push(event);
            next_after = sequence;
        }
        Ok(EventsPage {
            events,
            next_after,
            has_more,
        })
    }
}

fn validate_event_limit(limit: u32) -> Result<u32> {
    if !(1..=1000).contains(&limit) {
        return Err(Error::Invalid("limit must be 1..1000".into()));
    }
    Ok(limit + 1)
}
