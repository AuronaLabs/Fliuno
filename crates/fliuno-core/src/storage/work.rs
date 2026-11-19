//! Maintain small status counters from the narrow task status projection.
use super::*;

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

impl Kernel {
    pub fn work_snapshot(&self, request: &WorkSnapshotParams) -> Result<WorkSnapshot> {
        // Capture under the kernel lock used for task commits. The inbox
        // validates scope and budgets before either result is returned.
        let approvals = self.list_pending_approval_snapshot(&ApprovalInboxListParams {
            page: request.approval_page(),
            snapshot: None,
        })?;
        let (scope, owner) = match (&request.session_id, &request.workspace_id) {
            (Some(id), _) => (1, id.as_str()),
            (None, Some(id)) => (2, id.as_str()),
            _ => (0, ""),
        };
        let task_counts = self
            .db
            .prepare_cached(
                "SELECT running,waiting_approval,interrupted,completed,cancelled,failed
             FROM work_status_counts WHERE scope=?1 AND owner=?2",
            )?
            .query_row(params![scope, owner], |row| {
                Ok(TaskStatusCounts {
                    running: row.get::<_, i64>(0)? as u64,
                    waiting_approval: row.get::<_, i64>(1)? as u64,
                    interrupted: row.get::<_, i64>(2)? as u64,
                    completed: row.get::<_, i64>(3)? as u64,
                    cancelled: row.get::<_, i64>(4)? as u64,
                    failed: row.get::<_, i64>(5)? as u64,
                })
            })
            .optional()?
            .unwrap_or_default();
        Ok(WorkSnapshot {
            task_counts,
            approvals,
        })
    }
}
