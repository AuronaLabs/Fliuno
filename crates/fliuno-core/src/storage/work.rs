//! Maintain small status counters from the narrow task status projection.
use super::*;

mod session_counts;

const STATUSES: [TaskStatus; 6] = [
    TaskStatus::Running,
    TaskStatus::WaitingApproval,
    TaskStatus::Interrupted,
    TaskStatus::Completed,
    TaskStatus::Cancelled,
    TaskStatus::Failed,
];

pub(super) fn migrate(db: &mut Connection) -> Result<()> {
    let tx = db.transaction()?;
    let cols = STATUSES.map(TaskStatus::as_str).join(",");
    let definitions = STATUSES
        .map(|state| {
            let name = state.as_str();
            format!("{name} INTEGER NOT NULL CHECK({name}>=0)")
        })
        .join(",");
    let aggregates = STATUSES
        .map(|state| format!("SUM(status='{}')", state.as_str()))
        .join(",");
    tx.execute_batch(&format!(
        "CREATE TABLE IF NOT EXISTS work_status_counts(
             scope INTEGER NOT NULL CHECK(scope BETWEEN 0 AND 2),
             owner TEXT NOT NULL,
             {definitions},
             PRIMARY KEY(scope,owner)
         ) WITHOUT ROWID;
         DELETE FROM work_status_counts;
         INSERT INTO work_status_counts(scope,owner,{cols})
             SELECT 0,'',{aggregates} FROM task_summary_status HAVING COUNT(*)>0
             UNION ALL
             SELECT 1,session_id,{aggregates} FROM task_summary_status GROUP BY session_id
             UNION ALL
             SELECT 2,workspace_id,{aggregates} FROM task_summary_status GROUP BY workspace_id;"
    ))?;
    // One row per owner holds the entire breakdown. A transition updates each
    // owner once, even though it affects two different status counters.
    let new_values = STATUSES
        .map(|state| format!("NEW.status='{}'", state.as_str()))
        .join(",");
    let add = STATUSES
        .map(|state| {
            let name = state.as_str();
            format!("{name}={name}+excluded.{name}")
        })
        .join(",");
    let transition = STATUSES
        .map(|state| {
            let name = state.as_str();
            format!("{name}={name}+(NEW.status='{name}')-(OLD.status='{name}')")
        })
        .join(",");
    let remove = STATUSES
        .map(|state| {
            let name = state.as_str();
            format!("{name}={name}-(OLD.status='{name}')")
        })
        .join(",");
    // A task's owners are immutable. Legacy migration fixtures can have null
    // workspace IDs temporarily, until their workspace index is recreated.
    const OWNERS: &str = "(scope=0 AND owner='')
        OR (scope=1 AND owner=OLD.session_id)
        OR (scope=2 AND owner=COALESCE(OLD.workspace_id,''))";
    tx.execute_batch(&format!(
        "CREATE TRIGGER IF NOT EXISTS work_status_insert AFTER INSERT ON task_summary_status BEGIN
             INSERT INTO work_status_counts(scope,owner,{cols})
             VALUES (0,'',{new_values}), (1,NEW.session_id,{new_values}),
                    (2,COALESCE(NEW.workspace_id,''),{new_values})
             ON CONFLICT(scope,owner) DO UPDATE SET {add};
         END;
         CREATE TRIGGER IF NOT EXISTS work_status_update AFTER UPDATE OF status ON task_summary_status
         WHEN NEW.status<>OLD.status BEGIN
             UPDATE work_status_counts SET {transition} WHERE {OWNERS};
         END;
         CREATE TRIGGER IF NOT EXISTS work_status_delete AFTER DELETE ON task_summary_status BEGIN
             UPDATE work_status_counts SET {remove} WHERE {OWNERS};
         END;
         PRAGMA user_version=42;"
    ))?;
    tx.commit()?;
    Ok(())
}

pub(super) fn migrate_waiting_input(db: &mut Connection) -> Result<()> {
    let tx = db.transaction()?;
    let has_column: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_info('work_status_counts')
         WHERE name='waiting_input')",
        [],
        |row| row.get(0),
    )?;
    if !has_column {
        tx.execute_batch(
            "ALTER TABLE work_status_counts ADD COLUMN waiting_input INTEGER NOT NULL DEFAULT 0 CHECK(waiting_input>=0);",
        )?;
    }
    tx.execute_batch(
        "UPDATE work_status_counts SET waiting_input=CASE scope
             WHEN 0 THEN (SELECT COUNT(*) FROM task_summary_status WHERE status='waiting_input')
             WHEN 1 THEN (SELECT COUNT(*) FROM task_summary_status WHERE status='waiting_input' AND session_id=owner)
             ELSE (SELECT COUNT(*) FROM task_summary_status WHERE status='waiting_input' AND workspace_id=owner)
         END;
         DROP TRIGGER IF EXISTS work_status_insert;
         DROP TRIGGER IF EXISTS work_status_update;
         DROP TRIGGER IF EXISTS work_status_delete;",
    )?;
    let mut statuses = STATUSES.to_vec();
    statuses.push(TaskStatus::WaitingInput);
    install_triggers(&tx, &statuses)?;
    tx.pragma_update(None, "user_version", 54)?;
    tx.commit()?;
    Ok(())
}

fn install_triggers(tx: &rusqlite::Transaction<'_>, statuses: &[TaskStatus]) -> Result<()> {
    let cols = statuses
        .iter()
        .map(|state| state.as_str())
        .collect::<Vec<_>>()
        .join(",");
    let new_values = statuses
        .iter()
        .map(|state| format!("NEW.status='{}'", state.as_str()))
        .collect::<Vec<_>>()
        .join(",");
    let add = statuses
        .iter()
        .map(|state| {
            let name = state.as_str();
            format!("{name}={name}+excluded.{name}")
        })
        .collect::<Vec<_>>()
        .join(",");
    let transition = statuses
        .iter()
        .map(|state| {
            let name = state.as_str();
            format!("{name}={name}+(NEW.status='{name}')-(OLD.status='{name}')")
        })
        .collect::<Vec<_>>()
        .join(",");
    let remove = statuses
        .iter()
        .map(|state| {
            let name = state.as_str();
            format!("{name}={name}-(OLD.status='{name}')")
        })
        .collect::<Vec<_>>()
        .join(",");
    const OWNERS: &str = "(scope=0 AND owner='')
        OR (scope=1 AND owner=OLD.session_id)
        OR (scope=2 AND owner=COALESCE(OLD.workspace_id,''))";
    tx.execute_batch(&format!(
        "CREATE TRIGGER work_status_insert AFTER INSERT ON task_summary_status BEGIN
             INSERT INTO work_status_counts(scope,owner,{cols})
             VALUES (0,'',{new_values}), (1,NEW.session_id,{new_values}),
                    (2,COALESCE(NEW.workspace_id,''),{new_values})
             ON CONFLICT(scope,owner) DO UPDATE SET {add};
         END;
         CREATE TRIGGER work_status_update AFTER UPDATE OF status ON task_summary_status
         WHEN NEW.status<>OLD.status BEGIN
             UPDATE work_status_counts SET {transition} WHERE {OWNERS};
         END;
         CREATE TRIGGER work_status_delete AFTER DELETE ON task_summary_status BEGIN
             UPDATE work_status_counts SET {remove} WHERE {OWNERS};
         END;"
    ))?;
    Ok(())
}

impl Kernel {
    pub fn work_snapshot(&self, request: &WorkSnapshotParams) -> Result<WorkSnapshot> {
        if let Some(limit) = request.recent_task_limit
            && !(1..=MAX_WORK_RECENT_TASKS).contains(&limit)
        {
            return Err(Error::Invalid(format!(
                "recentTaskLimit must be 1 to {MAX_WORK_RECENT_TASKS}"
            )));
        }
        // The kernel lock orders the subscription handoff with local commits.
        // A read transaction also pins the inbox, counters and preview to one
        // SQLite view. The inbox validates scope and its own page budgets.
        let tx = self.db.unchecked_transaction()?;
        let approvals = self.read_pending_approval_snapshot(&ApprovalInboxListParams {
            page: request.approval_page(),
            snapshot: None,
        })?;
        let (scope, owner) = match (&request.session_id, &request.workspace_id) {
            (Some(id), _) => (1, id.as_str()),
            (None, Some(id)) => (2, id.as_str()),
            _ => (0, ""),
        };
        let task_counts = tx
            .prepare_cached(
                "SELECT running,waiting_approval,interrupted,completed,cancelled,failed,waiting_input
             FROM work_status_counts WHERE scope=?1 AND owner=?2",
            )?
            .query_row(params![scope, owner], |row| read_counts(row, 0))
            .optional()?
            .unwrap_or_default();
        let recent_tasks = request
            .recent_task_limit
            .map(|limit| recent_tasks(&tx, request, limit))
            .transpose()?;
        tx.commit()?;
        Ok(WorkSnapshot {
            task_counts,
            approvals,
            recent_tasks,
        })
    }
}

fn read_counts(row: &rusqlite::Row<'_>, offset: usize) -> rusqlite::Result<TaskStatusCounts> {
    Ok(TaskStatusCounts {
        running: row.get::<_, i64>(offset)? as u64,
        waiting_approval: row.get::<_, i64>(offset + 1)? as u64,
        interrupted: row.get::<_, i64>(offset + 2)? as u64,
        completed: row.get::<_, i64>(offset + 3)? as u64,
        cancelled: row.get::<_, i64>(offset + 4)? as u64,
        failed: row.get::<_, i64>(offset + 5)? as u64,
        waiting_input: row.get::<_, i64>(offset + 6)? as u64,
    })
}

fn recent_tasks(
    db: &Connection,
    request: &WorkSnapshotParams,
    limit: u32,
) -> Result<Vec<TaskSummary>> {
    // Drive each join from a reverse creation-order scan and look up only
    // the requested summaries. Session and workspace membership is immutable.
    let (sql, owner) = match (&request.session_id, &request.workspace_id) {
        (Some(id), _) => (
            "SELECT s.data FROM tasks t CROSS JOIN task_summaries s ON s.task_id=t.id
             WHERE t.session_id=?1 ORDER BY t.rowid DESC LIMIT ?2",
            Some(id),
        ),
        (None, Some(id)) => (
            "SELECT s.data FROM task_summary_status t CROSS JOIN task_summaries s ON s.task_id=t.task_id
             WHERE t.workspace_id=?1 ORDER BY t.task_row DESC LIMIT ?2",
            Some(id),
        ),
        _ => (
            "SELECT s.data FROM tasks t CROSS JOIN task_summaries s ON s.task_id=t.id
             ORDER BY t.rowid DESC LIMIT ?1",
            None,
        ),
    };
    let mut stmt = db.prepare_cached(sql)?;
    let mut rows = match owner {
        Some(id) => stmt.query(params![id, limit])?,
        None => stmt.query([limit])?,
    };
    let mut summaries = Vec::new();
    while let Some(row) = rows.next()? {
        let data = row.get_ref(0)?.as_str().map_err(rusqlite::Error::from)?;
        summaries.push(serde_json::from_str(data)?);
    }
    Ok(summaries)
}
