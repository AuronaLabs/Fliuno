//! Compact task list entries commit with the snapshot and its events.
use super::*;
use rusqlite::Transaction;

pub(super) fn migrate(db: &mut Connection) -> Result<()> {
    let tx = db.transaction()?;
    // Project older snapshots with their original query semantics. The full
    // task bodies and their row positions stay intact during this backfill.
    tx.execute_batch(
        r#"CREATE TABLE IF NOT EXISTS task_summaries(
            task_id TEXT PRIMARY KEY REFERENCES tasks(id),
            data TEXT NOT NULL
        ) WITHOUT ROWID;
        INSERT INTO task_summaries(task_id,data)
        SELECT id,json_object(
                'id',id,
                'sessionId',session_id,
                'status',json_extract(data,'$.status'),
                'backend',json(COALESCE(json_extract(data,'$.backend'),'{"type":"model"}')),
                'inputPreview',COALESCE(substr(json_extract(data,'$.messages[0].text'),1,160),''),
                'pendingApprovalCount',
                    CASE WHEN json_type(data,'$.pendingApproval')='object' THEN 1 ELSE 0 END
                    + COALESCE(json_array_length(data,'$.agentRun.pendingApprovals'),0),
                'modelOutcomeUnknown',json(CASE WHEN json_extract(data,'$.modelOutcomeUnknown')
                    THEN 'true' ELSE 'false' END),
                'agentOutcomeUnknown',json(CASE WHEN json_extract(data,'$.agentRun.outcomeUnknown')
                    THEN 'true' ELSE 'false' END)
             ) FROM tasks WHERE true
        ON CONFLICT(task_id) DO UPDATE SET data=excluded.data;
        PRAGMA user_version=18;"#,
    )?;
    tx.commit()?;
    Ok(())
}

/// Keep a narrow index for seeking sparse statuses in creation order.
pub(super) fn migrate_status_index(db: &mut Connection) -> Result<()> {
    let tx = db.transaction()?;
    tx.execute_batch(
        "CREATE TABLE IF NOT EXISTS task_summary_status(
            task_id TEXT PRIMARY KEY REFERENCES tasks(id) ON DELETE CASCADE,
            session_id TEXT NOT NULL,
            task_row INTEGER NOT NULL,
            status TEXT NOT NULL
        ) WITHOUT ROWID;
        CREATE INDEX IF NOT EXISTS task_summary_status_cursor
            ON task_summary_status(session_id,status,task_row);
        INSERT INTO task_summary_status(task_id,session_id,task_row,status)
        SELECT t.id,t.session_id,t.rowid,json_extract(s.data,'$.status')
            FROM task_summaries s JOIN tasks t ON t.id=s.task_id WHERE true
        ON CONFLICT(task_id) DO UPDATE SET status=excluded.status;
        CREATE TRIGGER IF NOT EXISTS task_summary_status_insert AFTER INSERT ON task_summaries
        BEGIN
            INSERT INTO task_summary_status(task_id,session_id,task_row,status)
            SELECT id,session_id,rowid,json_extract(NEW.data,'$.status') FROM tasks WHERE id=NEW.task_id
            ON CONFLICT(task_id) DO UPDATE SET status=excluded.status;
        END;
        CREATE TRIGGER IF NOT EXISTS task_summary_status_update AFTER UPDATE OF data ON task_summaries
        WHEN json_extract(OLD.data,'$.status')<>json_extract(NEW.data,'$.status')
        BEGIN
            UPDATE task_summary_status SET status=json_extract(NEW.data,'$.status')
                WHERE task_id=NEW.task_id;
        END;
        PRAGMA user_version=23;",
    )?;
    tx.commit()?;
    Ok(())
}

/// Workspace membership is immutable for a task. Index it once with the
/// existing creation cursor, then keep using the same status update trigger.
pub(super) fn migrate_workspace_scope(db: &mut Connection) -> Result<()> {
    let tx = db.transaction()?;
    let has_workspace: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_info('task_summary_status')
         WHERE name='workspace_id')",
        [],
        |row| row.get(0),
    )?;
    if !has_workspace {
        tx.execute_batch("ALTER TABLE task_summary_status ADD COLUMN workspace_id TEXT REFERENCES workspaces(id);")?;
    }
    tx.execute_batch(
        "UPDATE task_summary_status AS scoped
         SET workspace_id=(SELECT workspace_id FROM sessions WHERE id=scoped.session_id)
         WHERE workspace_id IS NULL;
         CREATE INDEX IF NOT EXISTS task_summary_workspace_cursor
            ON task_summary_status(workspace_id,task_row);
         CREATE INDEX IF NOT EXISTS task_summary_workspace_status_cursor
            ON task_summary_status(workspace_id,status,task_row);
         DROP TRIGGER task_summary_status_insert;
         CREATE TRIGGER task_summary_status_insert AFTER INSERT ON task_summaries
         BEGIN
            INSERT INTO task_summary_status(task_id,session_id,task_row,status,workspace_id)
            SELECT t.id,t.session_id,t.rowid,json_extract(NEW.data,'$.status'),s.workspace_id
                FROM tasks t JOIN sessions s ON s.id=t.session_id WHERE t.id=NEW.task_id
            ON CONFLICT(task_id) DO UPDATE SET status=excluded.status,workspace_id=excluded.workspace_id;
         END;
         PRAGMA user_version=24;",
    )?;
    tx.commit()?;
    Ok(())
}

pub(super) fn summarize(task: &Task) -> TaskSummary {
    let input_preview = match task.messages.first() {
        // SQLite substr() used to stop at the first NUL. Preserve that preview
        // for existing clients while limiting allocation to 160 scalars.
        Some(Message::User { text } | Message::Assistant { text, .. }) => text
            .chars()
            .take(160)
            .take_while(|ch| *ch != '\0')
            .collect(),
        Some(Message::UserParts { parts }) => parts
            .iter()
            .flat_map(|part| part.text_projection().chars())
            .take(160)
            .take_while(|ch| *ch != '\0')
            .collect(),
        _ => String::new(),
    };
    let run = task.agent_run.as_ref();
    TaskSummary {
        id: task.id.clone(),
        session_id: task.session_id.clone(),
        status: task.status,
        backend: task.backend.clone(),
        input_preview,
        pending_approval_count: u32::from(task.pending_approval.is_some())
            + run.map_or(0, |run| run.pending_approvals.len() as u32),
        model_outcome_unknown: task.model_outcome_unknown,
        agent_outcome_unknown: run.is_some_and(|run| run.outcome_unknown),
    }
}

/// True when the persisted summary was inserted or changed.
pub(super) fn record(tx: &Transaction<'_>, task: &Task) -> Result<bool> {
    let summary = summarize(task);
    let changed = tx
        .prepare_cached(
            "INSERT INTO task_summaries(task_id,data) VALUES(?1,?2)
         ON CONFLICT(task_id) DO UPDATE SET data=excluded.data
         WHERE task_summaries.data<>excluded.data",
        )?
        .execute(params![task.id, serde_json::to_string(&summary)?])?;
    Ok(changed != 0)
}

impl Kernel {
    pub(super) fn validate_task_lookup_batch(
        &self,
        request: &TaskSummaryBatchParams,
    ) -> Result<()> {
        if request.task_ids.is_empty() || request.task_ids.len() > MAX_TASK_SUMMARY_BATCH {
            return Err(Error::Invalid(format!(
                "taskIds must contain 1 to {MAX_TASK_SUMMARY_BATCH} entries"
            )));
        }
        if request.task_ids.iter().any(|id| {
            id.is_empty()
                || id.len() > MAX_TASK_SUMMARY_ID_BYTES
                || !id.bytes().all(|byte| byte.is_ascii_graphic())
        }) {
            return Err(Error::Invalid(format!(
                "task IDs must be 1 to {MAX_TASK_SUMMARY_ID_BYTES} visible ASCII bytes"
            )));
        }
        match (&request.workspace_id, &request.session_id) {
            (Some(_), Some(_)) => {
                return Err(Error::Invalid("select one task scope".into()));
            }
            (Some(id), None) => self.require_workspace(id)?,
            (None, Some(id)) => self.require_session(id)?,
            (None, None) => {}
        }
        Ok(())
    }

    /// Read only the compact summary projection, in one SQLite read view.
    pub fn get_task_summaries(&self, request: &TaskSummaryBatchParams) -> Result<TaskSummaryBatch> {
        self.validate_task_lookup_batch(request)?;
        // Drive indexed lookups from the bounded input array. Scope is checked
        // before joining the summary, so an excluded row's body is never read.
        // The array index restores request order without a SQL sort.
        let mut stmt = self.db.prepare_cached(
            "SELECT requested.key, summary.data, (SELECT COALESCE(MAX(sequence),0) FROM events)
             FROM json_each(?1) AS requested
             LEFT JOIN task_summary_status AS owner ON owner.task_id=requested.value
                AND (?2 IS NULL OR owner.workspace_id=?2)
                AND (?3 IS NULL OR owner.session_id=?3)
             LEFT JOIN task_summaries AS summary ON summary.task_id=owner.task_id",
        )?;
        let ids = serde_json::to_string(&request.task_ids)?;
        let rows = stmt.query(params![ids, request.workspace_id, request.session_id])?;
        read_summary_batch(rows, request.task_ids.len())
    }

    /// Reconcile submitted request keys in one session without reading task bodies.
    pub fn get_task_summaries_by_request(
        &self,
        request: &TaskRequestSummaryBatchParams,
    ) -> Result<TaskSummaryBatch> {
        if request.client_request_ids.is_empty()
            || request.client_request_ids.len() > MAX_TASK_SUMMARY_BATCH
        {
            return Err(Error::Invalid(format!(
                "clientRequestIds must contain 1 to {MAX_TASK_SUMMARY_BATCH} entries"
            )));
        }
        for key in &request.client_request_ids {
            crate::task::validate_key(key)?;
        }
        self.require_session(&request.session_id)?;
        // Include pre-receipt keys and continuations by reading the canonical
        // binding. The session and key together select an indexed row.
        let mut stmt = self.db.prepare_cached(
            "SELECT requested.key, summary.data, (SELECT COALESCE(MAX(sequence),0) FROM events)
             FROM json_each(?1) AS requested
             LEFT JOIN task_requests AS binding
                ON binding.session_id=?2 AND binding.request_key=requested.value
             LEFT JOIN task_summaries AS summary ON summary.task_id=binding.task_id",
        )?;
        let keys = serde_json::to_string(&request.client_request_ids)?;
        let rows = stmt.query(params![keys, request.session_id])?;
        read_summary_batch(rows, request.client_request_ids.len())
    }
}

fn read_summary_batch(mut rows: rusqlite::Rows<'_>, count: usize) -> Result<TaskSummaryBatch> {
    let mut items = vec![None; count];
    let mut through_sequence = None;
    while let Some(row) = rows.next()? {
        if through_sequence.is_none() {
            through_sequence = Some(row.get::<_, i64>(2)? as u64);
        }
        let index = usize::from(row.get::<_, u16>(0)?);
        let data = row
            .get_ref(1)?
            .as_str_or_null()
            .map_err(rusqlite::Error::from)?;
        items[index] = data.map(serde_json::from_str).transpose()?;
    }
    Ok(TaskSummaryBatch {
        items,
        through_sequence: through_sequence.ok_or(rusqlite::Error::QueryReturnedNoRows)?,
    })
}
