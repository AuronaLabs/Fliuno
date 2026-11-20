//! Compact inbox rows survive model approval recovery and are updated with each task snapshot.
use super::*;
use rusqlite::Transaction;

mod snapshot;
pub(super) use snapshot::migrate_snapshots;

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

pub(super) fn sql_timestamp(value: u64) -> Result<i64> {
    i64::try_from(value).map_err(|_| Error::Invalid("approval timestamp must fit i64".into()))
}

fn decode_kind(code: u8) -> Result<ApprovalKind> {
    match code {
        0 => Ok(ApprovalKind::ToolCall),
        1 => Ok(ApprovalKind::AgentPermission),
        2 => Ok(ApprovalKind::HostTool),
        _ => Err(Error::Conflict("invalid stored approval kind".into())),
    }
}

impl Kernel {
    pub fn get_pending_approval(
        &self,
        task_id: &str,
        approval_id: &str,
    ) -> Result<ApprovalDetails> {
        // Resolve the scope and expiry from the inbox before extracting the
        // requested value. SQLite returns just that approval to Rust.
        let mut select = self.db.prepare_cached(
            "SELECT p.kind, CASE p.kind
                 WHEN 0 THEN json_extract(t.data,'$.pendingApproval')
                 ELSE (SELECT a.value FROM json_each(t.data,'$.agentRun.pendingApprovals') a
                       WHERE json_extract(a.value,'$.id')=p.approval_id)
             END FROM pending_approvals p CROSS JOIN tasks t ON t.id=p.task_id
             WHERE p.approval_id=?1 AND p.task_id=?2
               AND (p.expires_at IS NULL OR p.expires_at>?3)",
        )?;
        let mut rows = select.query(params![
            approval_id,
            task_id,
            sql_timestamp(agent::now_ms()?)?
        ])?;
        let row = rows
            .next()?
            .ok_or_else(|| Error::NotFound("pending approval".into()))?;
        let data = row.get_ref(1)?.as_str().map_err(rusqlite::Error::from)?;
        Ok(match decode_kind(row.get(0)?)? {
            ApprovalKind::ToolCall => ApprovalDetails::ToolCall {
                approval: serde_json::from_str(data)?,
            },
            ApprovalKind::AgentPermission => ApprovalDetails::AgentPermission {
                approval: serde_json::from_str(data)?,
            },
            ApprovalKind::HostTool => ApprovalDetails::HostTool {
                approval: serde_json::from_str(data)?,
            },
        })
    }
    pub fn list_pending_approvals(
        &self,
        options: ApprovalListParams,
    ) -> Result<ObjectPage<ApprovalSummary>> {
        let (scope, filter) = self.approval_page_scope(&options)?;
        let ApprovalListParams {
            after,
            limit,
            target_bytes,
            ..
        } = options;
        // Scope clauses are fixed strings. Session and workspace index entries
        // carry the sequence as the implicit rowid and already provide the order.
        let sql = format!(
            "SELECT sequence,approval_id,task_id,session_id,kind,allow_once,expires_at
             FROM pending_approvals WHERE sequence>?2 {filter}
             AND (expires_at IS NULL OR expires_at>?3)
             ORDER BY sequence LIMIT ?4"
        );
        let mut select = self.db.prepare_cached(&sql)?;
        let rows = select.query(params![
            scope,
            after as i64,
            sql_timestamp(agent::now_ms()?)?,
            limit + 1
        ])?;
        read_approval_page(rows, after, limit, target_bytes)
    }

    fn approval_page_scope<'a>(
        &self,
        options: &'a ApprovalListParams,
    ) -> Result<(Option<&'a str>, &'static str)> {
        if options.after > i64::MAX as u64
            || !(1..=100).contains(&options.limit)
            || options
                .target_bytes
                .is_some_and(|bytes| !(1024..=MAX_APPROVAL_LIST_TARGET_BYTES).contains(&bytes))
        {
            return Err(Error::Invalid(
                "approval cursor, limit, or targetBytes is out of range".into(),
            ));
        }
        match (
            options.workspace_id.as_deref(),
            options.session_id.as_deref(),
        ) {
            (Some(_), Some(_)) => Err(Error::Invalid("choose only one approval list scope".into())),
            (Some(id), None) => {
                self.require_workspace(id)?;
                Ok((Some(id), "AND workspace_id=?1"))
            }
            (None, Some(id)) => {
                self.require_session(id)?;
                Ok((Some(id), "AND session_id=?1"))
            }
            (None, None) => Ok((None, "AND ?1 IS NULL")),
        }
    }
}

fn read_approval_page(
    mut rows: rusqlite::Rows<'_>,
    after: u64,
    limit: u32,
    target_bytes: Option<u32>,
) -> Result<ObjectPage<ApprovalSummary>> {
    let mut items = Vec::with_capacity(limit as usize);
    let mut next_after = after;
    let mut has_more = false;
    let mut bytes = 2;
    while let Some(row) = rows.next()? {
        if items.len() == limit as usize
            || target_bytes.is_some_and(|target| bytes >= target as usize)
        {
            has_more = true;
            break;
        }
        let kind = decode_kind(row.get(4)?)?;
        let item = ApprovalSummary {
            id: row.get(1)?,
            task_id: row.get(2)?,
            session_id: row.get(3)?,
            kind,
            allow_once_available: row.get(5)?,
            expires_at_unix_ms: row
                .get::<_, Option<i64>>(6)?
                .map(|timestamp| timestamp as u64),
        };
        if let Some(target) = target_bytes {
            bytes += crate::json_size(&item)? + usize::from(!items.is_empty());
            if bytes > target as usize && !items.is_empty() {
                has_more = true;
                break;
            }
        }
        next_after = row.get::<_, i64>(0)? as u64;
        items.push(item);
    }
    Ok(ObjectPage {
        items,
        next_after,
        has_more,
    })
}
