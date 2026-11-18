//! Compact inbox rows survive model approval recovery and are updated with each task snapshot.
use super::*;
use rusqlite::Transaction;

pub(super) fn migrate(db: &mut Connection) -> Result<()> {
    let tx = db.transaction()?;
    tx.execute_batch(
        "CREATE TABLE IF NOT EXISTS pending_approvals(
            sequence INTEGER PRIMARY KEY AUTOINCREMENT,
            approval_id TEXT NOT NULL UNIQUE,
            task_id TEXT NOT NULL REFERENCES tasks(id),
            session_id TEXT NOT NULL REFERENCES sessions(id),
            workspace_id TEXT NOT NULL REFERENCES workspaces(id),
            kind INTEGER NOT NULL CHECK(kind BETWEEN 0 AND 2),
            allow_once INTEGER NOT NULL,
            expires_at INTEGER CHECK(expires_at IS NULL OR expires_at >= 0)
        );
        CREATE INDEX IF NOT EXISTS pending_approvals_task ON pending_approvals(task_id);
        CREATE INDEX IF NOT EXISTS pending_approvals_session ON pending_approvals(session_id);
        CREATE INDEX IF NOT EXISTS pending_approvals_workspace ON pending_approvals(workspace_id);
        PRAGMA user_version=20;",
    )?;
    {
        // Drive the join from the small summaries. Read full snapshots only
        // for tasks with approvals, including legacy Agent snapshots.
        let mut statement = tx.prepare(
            "SELECT t.data FROM task_summaries s CROSS JOIN tasks t ON t.id=s.task_id
             WHERE json_extract(s.data,'$.pendingApprovalCount')>0 ORDER BY t.rowid",
        )?;
        let mut rows = statement.query([])?;
        while let Some(row) = rows.next()? {
            let data = row.get_ref(0)?.as_str().map_err(rusqlite::Error::from)?;
            record(&tx, &serde_json::from_str(data)?)?;
        }
    }
    tx.commit()?;
    Ok(())
}

// Pending identities are immutable. Update only added or removed entries so
// subsequent task updates never move an existing approval past a page cursor.
pub(super) fn record(tx: &Transaction<'_>, task: &Task) -> Result<()> {
    if task.status != TaskStatus::WaitingApproval {
        tx.prepare_cached("DELETE FROM pending_approvals WHERE task_id=?1")?
            .execute([&task.id])?;
        return Ok(());
    }
    let model = task
        .pending_approval
        .iter()
        .map(|a| (a.id.as_str(), 0, a.authority.is_some(), None));
    let agent = task
        .agent_run
        .iter()
        .flat_map(|run| &run.pending_approvals)
        .map(|a| {
            let kind = match a.source {
                AgentApprovalSource::AgentPermission => 1,
                AgentApprovalSource::HostTool => 2,
            };
            (
                a.id.as_str(),
                kind,
                a.allow_once_available,
                Some(a.expires_at_unix_ms),
            )
        });
    let pending = model.chain(agent);
    let mut select =
        tx.prepare_cached("SELECT approval_id FROM pending_approvals WHERE task_id=?1")?;
    let existing: Vec<String> = select
        .query_map([&task.id], |row| row.get(0))?
        .collect::<std::result::Result<_, _>>()?;
    let mut delete = tx.prepare_cached("DELETE FROM pending_approvals WHERE approval_id=?1")?;
    for id in &existing {
        if !pending.clone().any(|(active, _, _, _)| active == id) {
            delete.execute([id])?;
        }
    }
    let mut insert = tx.prepare_cached(
        "INSERT INTO pending_approvals(approval_id,task_id,session_id,workspace_id,kind,allow_once,expires_at)
         SELECT ?1,?2,id,workspace_id,?4,?5,?6 FROM sessions WHERE id=?3",
    )?;
    for (id, kind, allow_once, expires_at) in pending {
        if !existing.iter().any(|current| current == id) {
            insert.execute(params![
                id,
                task.id,
                task.session_id,
                kind,
                allow_once,
                expires_at.map(sql_timestamp).transpose()?
            ])?;
        }
    }
    Ok(())
}

fn sql_timestamp(value: u64) -> Result<i64> {
    i64::try_from(value).map_err(|_| Error::Invalid("approval timestamp must fit i64".into()))
}

impl Kernel {
    pub fn list_pending_approvals(
        &self,
        options: ApprovalListParams,
    ) -> Result<ObjectPage<ApprovalSummary>> {
        let ApprovalListParams {
            workspace_id,
            session_id,
            after,
            limit,
        } = options;
        if after > i64::MAX as u64 || !(1..=100).contains(&limit) {
            return Err(Error::Invalid(
                "after must fit i64 and approval list limit must be 1..100".into(),
            ));
        }
        let (scope, filter) = match (workspace_id.as_deref(), session_id.as_deref()) {
            (Some(_), Some(_)) => {
                return Err(Error::Invalid("choose only one approval list scope".into()));
            }
            (Some(id), None) => {
                self.require_workspace(id)?;
                (Some(id), "AND workspace_id=?1")
            }
            (None, Some(id)) => {
                self.require_session(id)?;
                (Some(id), "AND session_id=?1")
            }
            (None, None) => (None, "AND ?1 IS NULL"),
        };
        // Scope clauses are fixed strings. Session and workspace index entries
        // carry the sequence as the implicit rowid and already provide the order.
        let sql = format!(
            "SELECT sequence,approval_id,task_id,session_id,kind,allow_once,expires_at
             FROM pending_approvals WHERE sequence>?2 {filter}
             AND (expires_at IS NULL OR expires_at>?3)
             ORDER BY sequence LIMIT ?4"
        );
        let mut select = self.db.prepare_cached(&sql)?;
        let mut rows = select.query(params![
            scope,
            after as i64,
            sql_timestamp(agent::now_ms()?)?,
            limit + 1
        ])?;
        let mut items = Vec::with_capacity(limit as usize);
        let mut next_after = after;
        let mut has_more = false;
        while let Some(row) = rows.next()? {
            if items.len() == limit as usize {
                has_more = true;
                break;
            }
            next_after = row.get::<_, i64>(0)? as u64;
            let kind = match row.get::<_, u8>(4)? {
                0 => ApprovalKind::ToolCall,
                1 => ApprovalKind::AgentPermission,
                2 => ApprovalKind::HostTool,
                _ => return Err(Error::Conflict("invalid stored approval kind".into())),
            };
            items.push(ApprovalSummary {
                id: row.get(1)?,
                task_id: row.get(2)?,
                session_id: row.get(3)?,
                kind,
                allow_once_available: row.get(5)?,
                expires_at_unix_ms: row
                    .get::<_, Option<i64>>(6)?
                    .map(|timestamp| timestamp as u64),
            });
        }
        Ok(ObjectPage {
            items,
            next_after,
            has_more,
        })
    }
}
