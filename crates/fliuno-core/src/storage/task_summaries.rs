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

pub(super) fn record(tx: &Transaction<'_>, task: &Task) -> Result<()> {
    let input_preview = match task.messages.first() {
        // SQLite substr() used to stop at the first NUL. Preserve that preview
        // for existing clients while limiting allocation to 160 scalars.
        Some(Message::User { text } | Message::Assistant { text }) => text
            .chars()
            .take(160)
            .take_while(|ch| *ch != '\0')
            .collect(),
        _ => String::new(),
    };
    let run = task.agent_run.as_ref();
    let summary = TaskSummary {
        id: task.id.clone(),
        session_id: task.session_id.clone(),
        status: task.status,
        backend: task.backend.clone(),
        input_preview,
        pending_approval_count: u32::from(task.pending_approval.is_some())
            + run.map_or(0, |run| run.pending_approvals.len() as u32),
        model_outcome_unknown: task.model_outcome_unknown,
        agent_outcome_unknown: run.is_some_and(|run| run.outcome_unknown),
    };
    tx.execute(
        "INSERT INTO task_summaries(task_id,data) VALUES(?1,?2)
         ON CONFLICT(task_id) DO UPDATE SET data=excluded.data
         WHERE task_summaries.data<>excluded.data",
        params![task.id, serde_json::to_string(&summary)?],
    )?;
    Ok(())
}
