//! Read queries and pagination. Cursors are database-local opaque row positions.
use super::*;
use rusqlite::{OptionalExtension, params};
use serde::de::DeserializeOwned;

impl Kernel {
    pub(crate) fn find_task_request(&self, session: &str, key: &str) -> Result<Option<Task>> {
        let data=self.db.query_row("SELECT tasks.data FROM tasks JOIN task_requests ON tasks.id=task_requests.task_id WHERE task_requests.session_id=?1 AND request_key=?2",params![session,key],|r|r.get::<_,String>(0)).optional()?;
        data.map(|data| serde_json::from_str(&data).map_err(Error::from))
            .transpose()
    }

    pub fn list_workspaces(&self, after: u64, limit: u32) -> Result<ObjectPage<Workspace>> {
        self.object_page("SELECT rowid,data FROM workspaces WHERE ?1 IS NULL AND rowid>?2 ORDER BY rowid LIMIT ?3",None,after,limit)
    }

    pub fn list_sessions(
        &self,
        workspace: &str,
        after: u64,
        limit: u32,
    ) -> Result<ObjectPage<Session>> {
        self.list_sessions_filtered(workspace, after, limit, None)
    }

    pub fn list_sessions_filtered(
        &self,
        workspace: &str,
        after: u64,
        limit: u32,
        archived: Option<bool>,
    ) -> Result<ObjectPage<Session>> {
        self.require_workspace(workspace)?;
        // Filtered reads use sessions_workspace_archive, including the rowid range.
        let sql = match archived {
            None => {
                "SELECT rowid,data FROM sessions WHERE workspace_id=?1 AND rowid>?2
                 ORDER BY rowid LIMIT ?3"
            }
            Some(true) => {
                "SELECT rowid,data FROM sessions WHERE workspace_id=?1 AND rowid>?2
                 AND COALESCE(json_extract(data,'$.archived'),0)=1 ORDER BY rowid LIMIT ?3"
            }
            Some(false) => {
                "SELECT rowid,data FROM sessions WHERE workspace_id=?1 AND rowid>?2
                 AND COALESCE(json_extract(data,'$.archived'),0)=0 ORDER BY rowid LIMIT ?3"
            }
        };
        self.object_page(sql, Some(workspace), after, limit)
    }

    pub fn list_tasks(&self, session: &str, after: u64, limit: u32) -> Result<ObjectPage<Task>> {
        self.require_session(session)?;
        self.object_page(
            "SELECT rowid,data FROM tasks WHERE session_id=?1 AND rowid>?2 ORDER BY rowid LIMIT ?3",
            Some(session),
            after,
            limit,
        )
    }

    /// Read the same compact, committed projection used by the task catalog.
    pub fn get_task_summary(&self, task_id: &str) -> Result<TaskSummary> {
        let mut statement = self
            .db
            .prepare_cached("SELECT data FROM task_summaries WHERE task_id=?1")?;
        let mut rows = statement.query([task_id])?;
        let row = rows.next()?.ok_or_else(|| Error::NotFound("task".into()))?;
        let data = row.get_ref(0)?.as_str().map_err(rusqlite::Error::from)?;
        Ok(serde_json::from_str(data)?)
    }

    pub fn list_task_summaries(
        &self,
        session: &str,
        after: u64,
        limit: u32,
    ) -> Result<ObjectPage<TaskSummary>> {
        self.require_session(session)?;
        // Keep the row cursor shared with task/list and load only the summary.
        self.object_page(
            "SELECT tasks.rowid,summaries.data FROM tasks
             JOIN task_summaries AS summaries ON summaries.task_id=tasks.id
             WHERE tasks.session_id=?1 AND tasks.rowid>?2 ORDER BY tasks.rowid LIMIT ?3",
            Some(session),
            after,
            limit,
        )
    }

    fn object_page<T: DeserializeOwned>(
        &self,
        sql: &str,
        scope: Option<&str>,
        after: u64,
        limit: u32,
    ) -> Result<ObjectPage<T>> {
        if after > i64::MAX as u64 || !(1..=100).contains(&limit) {
            return Err(Error::Invalid(
                "after must fit i64 and object list limit must be 1..100".into(),
            ));
        }
        let mut statement = self.db.prepare_cached(sql)?;
        let mut rows = statement.query(params![scope, after as i64, limit + 1])?;
        let mut items = Vec::with_capacity(limit as usize);
        let mut next_after = after;
        let mut has_more = false;
        while let Some(row) = rows.next()? {
            if items.len() == limit as usize {
                has_more = true;
                break;
            }
            let data = row.get_ref(1)?.as_str().map_err(rusqlite::Error::from)?;
            items.push(serde_json::from_str(data)?);
            next_after = row.get::<_, i64>(0)? as u64;
        }
        Ok(ObjectPage {
            items,
            next_after,
            has_more,
        })
    }
}
