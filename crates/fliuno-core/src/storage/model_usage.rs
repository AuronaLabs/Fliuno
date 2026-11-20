//! Scope indexes for model usage. The committed event log owns the payload.
mod range;
mod totals;
use super::*;
use rusqlite::Transaction;
pub(super) use totals::migrate_totals;

pub(super) fn migrate(db: &mut Connection) -> Result<()> {
    let tx = db.transaction()?;
    tx.execute_batch(
        "CREATE TABLE IF NOT EXISTS model_usage(
            sequence INTEGER PRIMARY KEY REFERENCES events(sequence),
            task_id TEXT NOT NULL REFERENCES tasks(id),
            session_id TEXT NOT NULL REFERENCES sessions(id),
            workspace_id TEXT NOT NULL REFERENCES workspaces(id)
        );",
    )?;
    let has_task_index: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name='model_usage_index')",
        [],
        |row| row.get(0),
    )?;
    if has_task_index {
        // v37-v39 have an exact list of reports; do not scan or decode the log.
        tx.execute_batch(
            "INSERT INTO model_usage(sequence,task_id,session_id,workspace_id)
             SELECT u.sequence,u.task_id,s.id,s.workspace_id
             FROM model_usage_index u JOIN tasks t ON t.id=u.task_id
             JOIN sessions s ON s.id=t.session_id WHERE true
             ON CONFLICT(sequence) DO NOTHING;
             DROP TABLE model_usage_index;",
        )?;
    } else {
        // Pre-index databases may already contain reports in the event log.
        tx.execute_batch(
            "INSERT INTO model_usage(sequence,task_id,session_id,workspace_id)
             SELECT e.sequence,e.task_id,s.id,s.workspace_id
             FROM events e JOIN tasks t ON t.id=e.task_id JOIN sessions s ON s.id=t.session_id
             WHERE json_extract(e.data,'$.type')='model_usage_reported'
             ON CONFLICT(sequence) DO NOTHING;",
        )?;
    }
    tx.execute_batch(
        "CREATE INDEX IF NOT EXISTS model_usage_task ON model_usage(task_id,sequence);
         CREATE INDEX IF NOT EXISTS model_usage_session ON model_usage(session_id,sequence);
         CREATE INDEX IF NOT EXISTS model_usage_workspace ON model_usage(workspace_id,sequence);
         PRAGMA user_version=40;",
    )?;
    tx.commit()?;
    Ok(())
}

pub(super) fn record(
    tx: &Transaction<'_>,
    task: &Task,
    sequence: i64,
    usage: &ModelUsage,
) -> Result<()> {
    let workspace_id: String = tx
        .prepare_cached(
            "INSERT INTO model_usage(sequence,task_id,session_id,workspace_id)
         VALUES (?1,?2,?3,(SELECT workspace_id FROM sessions WHERE id=?3))
         RETURNING workspace_id",
        )?
        .query_row(params![sequence, task.id, task.session_id], |row| {
            row.get(0)
        })?;
    totals::record(tx, [&task.id, &task.session_id, &workspace_id], usage)
}

#[derive(Clone, Copy)]
enum UsageScope<'a> {
    Task(&'a str),
    Session(&'a str),
    Workspace(&'a str),
}

impl<'a> UsageScope<'a> {
    fn resolve(self, kernel: &Kernel) -> Result<(&'a str, &'static str)> {
        match self {
            Self::Task(id) => {
                kernel.require_task(id)?;
                Ok((id, "task_id"))
            }
            Self::Session(id) => {
                kernel.require_session(id)?;
                Ok((id, "session_id"))
            }
            Self::Workspace(id) => {
                kernel.require_workspace(id)?;
                Ok((id, "workspace_id"))
            }
        }
    }
}

impl Kernel {
    pub fn list_model_usage_events(&self, request: &EventsListParams) -> Result<EventsPage> {
        self.list_usage_scope(
            UsageScope::Task(&request.task_id),
            request.after,
            request.through,
            request.limit,
            request.target_bytes,
        )
    }

    pub fn list_session_model_usage_events(
        &self,
        request: &EventsListSessionParams,
    ) -> Result<EventsPage> {
        self.list_usage_scope(
            UsageScope::Session(&request.session_id),
            request.after,
            request.through,
            request.limit,
            request.target_bytes,
        )
    }

    pub fn list_workspace_model_usage_events(
        &self,
        request: &EventsListWorkspaceParams,
    ) -> Result<EventsPage> {
        self.list_usage_scope(
            UsageScope::Workspace(&request.workspace_id),
            request.after,
            request.through,
            request.limit,
            request.target_bytes,
        )
    }

    fn list_usage_scope(
        &self,
        scope: UsageScope<'_>,
        after: u64,
        through: Option<u64>,
        limit: u32,
        target_bytes: Option<u32>,
    ) -> Result<EventsPage> {
        let (owner, column) = scope.resolve(self)?;
        // Seek in the scope's covering index and fetch only matching event rows.
        // CROSS JOIN keeps the scope range on the outside of the join.
        self.query_event_page(
            &format!(
                "SELECT e.sequence,e.task_id,e.data
                 FROM model_usage u CROSS JOIN events e ON e.sequence=u.sequence
                 WHERE u.{column}=?1 AND u.sequence>?2 AND u.sequence<=?3
                 ORDER BY u.sequence LIMIT ?4"
            ),
            params![
                owner,
                validate_event_cursor(after)?,
                through
                    .map(validate_event_cursor)
                    .transpose()?
                    .unwrap_or(i64::MAX),
                replay::validate_event_limit(limit)?
            ],
            after,
            limit,
            target_bytes,
        )
    }
}
