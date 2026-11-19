//! Read queries and pagination. Cursors are database-local opaque row positions.
use super::*;
use rusqlite::{OptionalExtension, params};
use serde::{Serialize, de::DeserializeOwned};

pub const MAX_TASK_LIST_TARGET_BYTES: u32 = 8 * 1024 * 1024;

impl Kernel {
    /// Read the saved workspace identity without accessing the host path.
    pub fn get_workspace(&self, workspace_id: &str) -> Result<Workspace> {
        let mut statement = self
            .db
            .prepare_cached("SELECT data FROM workspaces WHERE id=?1")?;
        let mut rows = statement.query([workspace_id])?;
        let row = rows
            .next()?
            .ok_or_else(|| Error::NotFound("workspace".into()))?;
        let data = row.get_ref(0)?.as_str().map_err(rusqlite::Error::from)?;
        Ok(serde_json::from_str(data)?)
    }

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
        self.list_tasks_with_target(session, after, limit, None)
    }

    pub fn list_tasks_with_target(
        &self,
        session: &str,
        after: u64,
        limit: u32,
        target_bytes: Option<u32>,
    ) -> Result<ObjectPage<Task>> {
        validate_task_list_target(target_bytes)?;
        self.require_session(session)?;
        let (cursor, fetch) = page_range(after, limit)?;
        self.query_object_page(
            "SELECT rowid,data FROM tasks WHERE session_id=?1 AND rowid>?2 ORDER BY rowid LIMIT ?3",
            params![session, cursor, fetch],
            after,
            limit,
            target_bytes,
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
        self.list_task_summaries_filtered(session, after, limit, None)
    }

    pub fn list_task_summaries_filtered(
        &self,
        session: &str,
        after: u64,
        limit: u32,
        status: Option<TaskStatus>,
    ) -> Result<ObjectPage<TaskSummary>> {
        self.list_task_summaries_with_target(session, after, limit, status, None)
    }

    pub fn list_task_summaries_with_target(
        &self,
        session: &str,
        after: u64,
        limit: u32,
        status: Option<TaskStatus>,
        target_bytes: Option<u32>,
    ) -> Result<ObjectPage<TaskSummary>> {
        validate_task_list_target(target_bytes)?;
        self.require_session(session)?;
        if let Some(status) = status {
            let (cursor, fetch) = page_range(after, limit)?;
            return self.query_object_page(
                "SELECT scoped.task_row,summaries.data FROM task_summary_status scoped
                 JOIN task_summaries summaries ON summaries.task_id=scoped.task_id
                 WHERE scoped.session_id=?1 AND scoped.status=?2 AND scoped.task_row>?3
                 ORDER BY scoped.task_row LIMIT ?4",
                params![session, status.as_str(), cursor, fetch],
                after,
                limit,
                target_bytes,
            );
        }
        // Keep the row cursor shared with task/list and load only the summary.
        let (cursor, fetch) = page_range(after, limit)?;
        self.query_object_page(
            "SELECT tasks.rowid,summaries.data FROM tasks
             JOIN task_summaries AS summaries ON summaries.task_id=tasks.id
             WHERE tasks.session_id=?1 AND tasks.rowid>?2 ORDER BY tasks.rowid LIMIT ?3",
            params![session, cursor, fetch],
            after,
            limit,
            target_bytes,
        )
    }

    /// Page compact snapshots across all sessions in a workspace. The
    /// cursor is the same creation position used by the session catalog.
    pub fn list_workspace_task_summaries(
        &self,
        workspace: &str,
        after: u64,
        limit: u32,
        status: Option<TaskStatus>,
    ) -> Result<ObjectPage<TaskSummary>> {
        self.list_workspace_task_summaries_with_target(workspace, after, limit, status, None)
    }

    pub fn list_workspace_task_summaries_with_target(
        &self,
        workspace: &str,
        after: u64,
        limit: u32,
        status: Option<TaskStatus>,
        target_bytes: Option<u32>,
    ) -> Result<ObjectPage<TaskSummary>> {
        self.list_workspace_task_summaries_ordered(
            workspace,
            after,
            limit,
            status,
            target_bytes,
            WorkspaceTaskSummaryOrder::OldestFirst,
        )
    }

    pub fn list_workspace_task_summaries_ordered(
        &self,
        workspace: &str,
        after: u64,
        limit: u32,
        status: Option<TaskStatus>,
        target_bytes: Option<u32>,
        order: WorkspaceTaskSummaryOrder,
    ) -> Result<ObjectPage<TaskSummary>> {
        validate_task_list_target(target_bytes)?;
        self.require_workspace(workspace)?;
        let (cursor, fetch) = page_range(after, limit)?;
        use WorkspaceTaskSummaryOrder::{NewestFirst, OldestFirst};
        // Reverse scans use an inclusive ceiling one below the last row. For
        // the first page, a zero cursor means start at the end of the index.
        let bound = match (order, cursor) {
            (OldestFirst, _) => cursor,
            (NewestFirst, 0) => i64::MAX,
            (NewestFirst, _) => cursor - 1,
        };
        if let Some(status) = status {
            let sql = match order {
                OldestFirst => {
                    "SELECT scoped.task_row,summaries.data FROM task_summary_status scoped
                 JOIN task_summaries summaries ON summaries.task_id=scoped.task_id
                 WHERE scoped.workspace_id=?1 AND scoped.status=?2 AND scoped.task_row>?3
                 ORDER BY scoped.task_row LIMIT ?4"
                }
                NewestFirst => {
                    "SELECT scoped.task_row,summaries.data FROM task_summary_status scoped
                 JOIN task_summaries summaries ON summaries.task_id=scoped.task_id
                 WHERE scoped.workspace_id=?1 AND scoped.status=?2 AND scoped.task_row<=?3
                 ORDER BY scoped.task_row DESC LIMIT ?4"
                }
            };
            return self.query_object_page(
                sql,
                params![workspace, status.as_str(), bound, fetch],
                after,
                limit,
                target_bytes,
            );
        }
        let sql = match order {
            OldestFirst => {
                "SELECT scoped.task_row,summaries.data FROM task_summary_status scoped
             JOIN task_summaries summaries ON summaries.task_id=scoped.task_id
             WHERE scoped.workspace_id=?1 AND scoped.task_row>?2
             ORDER BY scoped.task_row LIMIT ?3"
            }
            NewestFirst => {
                "SELECT scoped.task_row,summaries.data FROM task_summary_status scoped
             JOIN task_summaries summaries ON summaries.task_id=scoped.task_id
             WHERE scoped.workspace_id=?1 AND scoped.task_row<=?2
             ORDER BY scoped.task_row DESC LIMIT ?3"
            }
        };
        self.query_object_page(
            sql,
            params![workspace, bound, fetch],
            after,
            limit,
            target_bytes,
        )
    }

    fn object_page<T: DeserializeOwned + Serialize>(
        &self,
        sql: &str,
        scope: Option<&str>,
        after: u64,
        limit: u32,
    ) -> Result<ObjectPage<T>> {
        let (cursor, fetch) = page_range(after, limit)?;
        self.query_object_page(sql, params![scope, cursor, fetch], after, limit, None)
    }

    fn query_object_page<T: DeserializeOwned + Serialize>(
        &self,
        sql: &str,
        params: impl rusqlite::Params,
        after: u64,
        limit: u32,
        target_bytes: Option<u32>,
    ) -> Result<ObjectPage<T>> {
        let mut statement = self.db.prepare_cached(sql)?;
        let mut rows = statement.query(params)?;
        let mut items = Vec::with_capacity(limit as usize);
        let mut next_after = after;
        let mut has_more = false;
        let mut bytes = 2; // Array brackets.
        while let Some(row) = rows.next()? {
            if items.len() == limit as usize
                || target_bytes.is_some_and(|target| bytes >= target as usize)
            {
                has_more = true;
                break;
            }
            let data = row.get_ref(1)?.as_str().map_err(rusqlite::Error::from)?;
            let item = serde_json::from_str(data)?;
            if let Some(target) = target_bytes {
                bytes += json_size(&item)? + usize::from(!items.is_empty());
                if bytes > target as usize && !items.is_empty() {
                    // Keep the candidate for the next page, even if it is large.
                    has_more = true;
                    break;
                }
            }
            items.push(item);
            next_after = row.get::<_, i64>(0)? as u64;
        }
        Ok(ObjectPage {
            items,
            next_after,
            has_more,
        })
    }
}

fn validate_task_list_target(target_bytes: Option<u32>) -> Result<()> {
    if target_bytes.is_some_and(|bytes| !(1024..=MAX_TASK_LIST_TARGET_BYTES).contains(&bytes)) {
        return Err(Error::Invalid("targetBytes must be 1024..8388608".into()));
    }
    Ok(())
}

pub(crate) fn page_range(after: u64, limit: u32) -> Result<(i64, u32)> {
    if after > i64::MAX as u64 || !(1..=100).contains(&limit) {
        return Err(Error::Invalid(
            "after must fit i64 and object list limit must be 1..100".into(),
        ));
    }
    Ok((after as i64, limit + 1))
}
