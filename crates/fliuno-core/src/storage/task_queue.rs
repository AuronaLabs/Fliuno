//! Durable FIFO drafts. A dispatch receipt and its task share one SQLite commit.
use super::*;
use rusqlite::TransactionBehavior;

pub(super) fn migrate(db: &mut Connection) -> Result<()> {
    let tx = db.transaction()?;
    tx.execute_batch(
        "CREATE TABLE IF NOT EXISTS task_queue(
            position INTEGER PRIMARY KEY,
            id TEXT NOT NULL UNIQUE,
            session_id TEXT NOT NULL REFERENCES sessions(id),
            request_key TEXT NOT NULL,
            request_digest BLOB NOT NULL,
            preview TEXT NOT NULL,
            plan_only INTEGER NOT NULL,
            payload TEXT,
            task_id TEXT REFERENCES tasks(id),
            UNIQUE(session_id,request_key),
            CHECK(payload IS NULL OR task_id IS NULL)
        );
        CREATE INDEX IF NOT EXISTS task_queue_pending ON task_queue(session_id,position)
            WHERE payload IS NOT NULL;
        PRAGMA user_version=75;",
    )?;
    tx.commit()?;
    Ok(())
}

fn archived(db: &Connection, session: &str) -> Result<bool> {
    db.query_row(
        "SELECT COALESCE(json_extract(data,'$.archived'),0) FROM sessions WHERE id=?1",
        [session],
        |row| row.get(0),
    )
    .optional()?
    .ok_or_else(|| Error::NotFound("session".into()))
}

fn blocking_task(db: &Connection, session: &str) -> Result<Option<String>> {
    Ok(db
        .query_row(
            "SELECT task_id FROM task_summary_status WHERE session_id=?1
         AND status IN ('running','waiting_approval','waiting_input','interrupted')
         LIMIT 1",
            [session],
            |row| row.get(0),
        )
        .optional()?)
}

pub(super) fn require_dispatch(db: &Connection, session: &str, queue_id: &str) -> Result<()> {
    if archived(db, session)? {
        return Err(Error::Conflict("session is archived".into()));
    }
    let head: Option<String> = db
        .query_row(
            "SELECT id FROM task_queue WHERE session_id=?1 AND payload IS NOT NULL
         ORDER BY position LIMIT 1",
            [session],
            |row| row.get(0),
        )
        .optional()?;
    if head.as_deref() != Some(queue_id) {
        return Err(Error::Conflict(
            "select the first pending prompt to start it".into(),
        ));
    }
    if let Some(id) = blocking_task(db, session)? {
        return Err(Error::Conflict(format!(
            "resolve task {id} before starting the next prompt"
        )));
    }
    Ok(())
}

pub(super) fn record_dispatch(db: &Connection, task: &Task, queue_id: &str) -> Result<()> {
    let count = db.execute(
        "UPDATE task_queue SET payload=NULL,task_id=?1
         WHERE id=?2 AND session_id=?3 AND payload IS NOT NULL",
        params![task.id, queue_id, task.session_id],
    )?;
    if count != 1 {
        return Err(Error::Conflict(
            "queued prompt changed before dispatch".into(),
        ));
    }
    Ok(())
}

fn decode_item(row: &rusqlite::Row<'_>) -> rusqlite::Result<TaskQueueItem> {
    let status = if let Some(task_id) = row.get(3)? {
        TaskQueueState::Started { task_id }
    } else if row.get::<_, bool>(4)? {
        TaskQueueState::Pending
    } else {
        TaskQueueState::Cancelled
    };
    Ok(TaskQueueItem {
        queue_id: row.get(0)?,
        input_preview: row.get(1)?,
        plan_only: row.get(2)?,
        status,
    })
}

fn item(db: &Connection, target: &TaskQueueItemParams) -> Result<TaskQueueItem> {
    db.query_row(
        "SELECT id,preview,plan_only,task_id,payload IS NOT NULL FROM task_queue
         WHERE session_id=?1 AND id=?2",
        params![target.session_id, target.queue_id],
        decode_item,
    )
    .optional()?
    .ok_or_else(|| Error::NotFound("prompt in selected session".into()))
}

pub(crate) struct NewQueuedPrompt<'a> {
    pub key: &'a str,
    pub payload: &'a str,
    pub digest: [u8; 32],
    pub preview: String,
    pub plan_only: bool,
}

impl Kernel {
    pub fn list_task_queue(&self, session: &str) -> Result<TaskQueue> {
        let tx = self.db.unchecked_transaction()?;
        let archived = archived(&tx, session)?;
        let pending = tx
            .prepare_cached(
                "SELECT id,preview,plan_only,task_id,1 FROM task_queue
             WHERE session_id=?1 AND payload IS NOT NULL ORDER BY position",
            )?
            .query_map([session], decode_item)?
            .collect::<rusqlite::Result<_>>()?;
        let blocking_task_id = blocking_task(&tx, session)?;
        tx.commit()?;
        Ok(TaskQueue {
            session_id: session.into(),
            pending,
            blocking_task_id,
            archived,
        })
    }

    pub(crate) fn store_queued_prompt(
        &mut self,
        session: &str,
        prompt: NewQueuedPrompt<'_>,
    ) -> Result<TaskQueueItem> {
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let existing = tx
            .query_row(
                "SELECT id,preview,plan_only,task_id,payload IS NOT NULL,request_digest
             FROM task_queue WHERE session_id=?1 AND request_key=?2",
                params![session, prompt.key],
                |row| Ok((decode_item(row)?, row.get::<_, Vec<u8>>(5)?)),
            )
            .optional()?;
        if let Some((item, digest)) = existing {
            if digest != prompt.digest {
                return Err(Error::Conflict(
                    "queued prompt request ID is already in use".into(),
                ));
            }
            return Ok(item);
        }
        if archived(&tx, session)? {
            return Err(Error::Conflict("session is archived".into()));
        }
        let pending: u32 = tx.query_row(
            "SELECT count(*) FROM task_queue WHERE session_id=?1 AND payload IS NOT NULL",
            [session],
            |row| row.get(0),
        )?;
        if pending as usize >= MAX_SESSION_PENDING_PROMPTS {
            return Err(Error::Conflict("the session prompt queue is full".into()));
        }
        let id = new_id("prompt");
        tx.execute(
            "INSERT INTO task_queue(id,session_id,request_key,request_digest,preview,plan_only,payload)
             VALUES(?1,?2,?3,?4,?5,?6,?7)",
            params![id, session, prompt.key, prompt.digest.as_slice(), prompt.preview,
                prompt.plan_only, prompt.payload],
        )?;
        tx.commit()?;
        Ok(TaskQueueItem {
            queue_id: id,
            input_preview: prompt.preview,
            plan_only: prompt.plan_only,
            status: TaskQueueState::Pending,
        })
    }

    pub fn queued_prompt_to_start(
        &self,
        target: &TaskQueueItemParams,
    ) -> Result<(TaskQueueItem, Option<TaskStartParams>)> {
        let tx = self.db.unchecked_transaction()?;
        let current = item(&tx, target)?;
        if current.status != TaskQueueState::Pending {
            return Ok((current, None));
        }
        require_dispatch(&tx, &target.session_id, &target.queue_id)?;
        let payload: String = tx.query_row(
            "SELECT payload FROM task_queue WHERE session_id=?1 AND id=?2",
            params![target.session_id, target.queue_id],
            |row| row.get(0),
        )?;
        tx.commit()?;
        let mut params: TaskStartParams = serde_json::from_str(&payload)?;
        // Queue receipts carry their own request identity, independent of direct submission.
        params.client_request_id = None;
        Ok((current, Some(params)))
    }

    pub fn cancel_queued_prompt(&mut self, target: &TaskQueueItemParams) -> Result<TaskQueueItem> {
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut current = item(&tx, target)?;
        match current.status {
            TaskQueueState::Started { .. } => {
                return Err(Error::Conflict(
                    "prompt has started; cancel the task instead".into(),
                ));
            }
            TaskQueueState::Cancelled => return Ok(current),
            TaskQueueState::Pending => {}
        }
        tx.execute(
            "UPDATE task_queue SET payload=NULL WHERE session_id=?1 AND id=?2",
            params![target.session_id, target.queue_id],
        )?;
        tx.commit()?;
        current.status = TaskQueueState::Cancelled;
        Ok(current)
    }
}
