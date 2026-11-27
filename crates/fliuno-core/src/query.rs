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
        self.list_sessions_ordered(
            workspace,
            after,
            limit,
            archived,
            SessionListOrder::OldestFirst,
        )
    }

    /// Catalogue position is by creation row, independent of titles and archive changes.
    pub fn list_sessions_ordered(
        &self,
        workspace: &str,
        after: u64,
        limit: u32,
        archived: Option<bool>,
        order: SessionListOrder,
    ) -> Result<ObjectPage<Session>> {
        use SessionListOrder::{NewestFirst, OldestFirst};
        self.require_workspace(workspace)?;
        let (cursor, fetch) = page_range(after, limit)?;
        let bound = match (order, cursor) {
            (OldestFirst, _) => cursor,
            (NewestFirst, 0) => i64::MAX,
            (NewestFirst, _) => cursor - 1,
        };
        if let Some(archived) = archived {
            // The equality on the indexed expression lets SQLite seek to the
            // rowid bound within just this workspace and archive status.
            let sql = match order {
                OldestFirst => {
                    "SELECT rowid,data FROM sessions WHERE workspace_id=?1
                    AND COALESCE(json_extract(data,'$.archived'),0)=?4 AND rowid>?2
                    ORDER BY rowid LIMIT ?3"
                }
                NewestFirst => {
                    "SELECT rowid,data FROM sessions WHERE workspace_id=?1
                    AND COALESCE(json_extract(data,'$.archived'),0)=?4 AND rowid<=?2
                    ORDER BY rowid DESC LIMIT ?3"
                }
            };
            return self.query_object_page(
                sql,
                params![workspace, bound, fetch, i64::from(archived)],
                after,
                limit,
                None,
            );
        }
        let sql = match order {
            OldestFirst => {
                "SELECT rowid,data FROM sessions WHERE workspace_id=?1 AND rowid>?2
                ORDER BY rowid LIMIT ?3"
            }
            NewestFirst => {
                "SELECT rowid,data FROM sessions WHERE workspace_id=?1 AND rowid<=?2
                ORDER BY rowid DESC LIMIT ?3"
            }
        };
        self.query_object_page(sql, params![workspace, bound, fetch], after, limit, None)
    }

    pub fn list_session_catalog(&self, params: &SessionListParams) -> Result<ObjectPage<Session>> {
        let order = params.order.unwrap_or_default();
        let Some(query) = params.query.as_deref() else {
            return self.list_sessions_ordered(
                &params.workspace_id,
                params.after,
                params.limit,
                params.archived,
                order,
            );
        };
        let query = query.trim();
        if query.is_empty()
            || query.chars().take(MAX_SESSION_TITLE_CHARS + 1).count() > MAX_SESSION_TITLE_CHARS
            || query.chars().any(char::is_control)
        {
            return Err(Error::Invalid(
                "session query must contain 1 to 120 printable characters".into(),
            ));
        }
        self.require_workspace(&params.workspace_id)?;
        let (cursor, fetch) = page_range(params.after, params.limit)?;
        let (bound, predicate, direction) = match order {
            SessionListOrder::OldestFirst => (cursor, ">", "ASC"),
            SessionListOrder::NewestFirst => (
                if cursor == 0 { i64::MAX } else { cursor - 1 },
                "<=",
                "DESC",
            ),
        };
        let archive_filter = if params.archived.is_some() {
            "AND COALESCE(json_extract(data,'$.archived'),0)=?5"
        } else {
            "AND ?5 IS NULL"
        };
        let sql = format!(
            "SELECT rowid,data FROM sessions WHERE workspace_id=?1 AND rowid{predicate}?2
            {archive_filter} AND (instr(lower(id),lower(?4))>0
                OR instr(lower(json_extract(data,'$.title')),lower(?4))>0)
            ORDER BY rowid {direction} LIMIT ?3"
        );
        self.query_object_page(
            &sql,
            params![params.workspace_id, bound, fetch, query, params.archived],
            params.after,
            params.limit,
            None,
        )
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

    /// Find a request's current summary without reading the task snapshot or context.
    pub fn get_task_summary_by_request(&self, session_id: &str, key: &str) -> Result<TaskSummary> {
        crate::task::validate_key(key)?;
        let mut statement = self.db.prepare_cached(
            "SELECT s.data FROM task_requests r JOIN task_summaries s ON s.task_id=r.task_id
             WHERE r.session_id=?1 AND r.request_key=?2",
        )?;
        let mut rows = statement.query(params![session_id, key])?;
        let row = rows
            .next()?
            .ok_or_else(|| Error::NotFound("task request".into()))?;
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
        self.list_task_summaries_ordered(
            session,
            after,
            limit,
            status,
            target_bytes,
            TaskSummaryOrder::OldestFirst,
        )
    }

    /// Page compact summaries within a session using its task creation positions.
    pub fn list_task_summaries_ordered(
        &self,
        session: &str,
        after: u64,
        limit: u32,
        status: Option<TaskStatus>,
        target_bytes: Option<u32>,
        order: TaskSummaryOrder,
    ) -> Result<ObjectPage<TaskSummary>> {
        use TaskSummaryOrder::{NewestFirst, OldestFirst};
        validate_task_list_target(target_bytes)?;
        self.require_session(session)?;
        let (cursor, fetch) = page_range(after, limit)?;
        let bound = task_catalog_bound(order, cursor);
        if let Some(status) = status {
            let sql = match order {
                OldestFirst => {
                    "SELECT scoped.task_row,summaries.data FROM task_summary_status scoped
                 CROSS JOIN task_summaries summaries ON summaries.task_id=scoped.task_id
                 WHERE scoped.session_id=?1 AND scoped.status=?2 AND scoped.task_row>?3
                 ORDER BY scoped.task_row LIMIT ?4"
                }
                NewestFirst => {
                    "SELECT scoped.task_row,summaries.data FROM task_summary_status scoped
                 CROSS JOIN task_summaries summaries ON summaries.task_id=scoped.task_id
                 WHERE scoped.session_id=?1 AND scoped.status=?2 AND scoped.task_row<=?3
                 ORDER BY scoped.task_row DESC LIMIT ?4"
                }
            };
            return self.query_object_page(
                sql,
                params![session, status.as_str(), bound, fetch],
                after,
                limit,
                target_bytes,
            );
        }
        // The session index includes rowid, so both orders seek to the cursor.
        let sql = match order {
            OldestFirst => {
                "SELECT tasks.rowid,summaries.data FROM tasks
             CROSS JOIN task_summaries AS summaries ON summaries.task_id=tasks.id
             WHERE tasks.session_id=?1 AND tasks.rowid>?2 ORDER BY tasks.rowid LIMIT ?3"
            }
            NewestFirst => {
                "SELECT tasks.rowid,summaries.data FROM tasks
             CROSS JOIN task_summaries AS summaries ON summaries.task_id=tasks.id
             WHERE tasks.session_id=?1 AND tasks.rowid<=?2 ORDER BY tasks.rowid DESC LIMIT ?3"
            }
        };
        self.query_object_page(
            sql,
            params![session, bound, fetch],
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
            TaskSummaryOrder::OldestFirst,
        )
    }

    pub fn list_workspace_task_summaries_ordered(
        &self,
        workspace: &str,
        after: u64,
        limit: u32,
        status: Option<TaskStatus>,
        target_bytes: Option<u32>,
        order: TaskSummaryOrder,
    ) -> Result<ObjectPage<TaskSummary>> {
        validate_task_list_target(target_bytes)?;
        self.require_workspace(workspace)?;
        let (cursor, fetch) = page_range(after, limit)?;
        use TaskSummaryOrder::{NewestFirst, OldestFirst};
        let bound = task_catalog_bound(order, cursor);
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

// Reverse scans use an inclusive ceiling one below the last row. A zero
// cursor starts from the end of the index.
fn task_catalog_bound(order: TaskSummaryOrder, cursor: i64) -> i64 {
    match (order, cursor) {
        (TaskSummaryOrder::OldestFirst, _) => cursor,
        (TaskSummaryOrder::NewestFirst, 0) => i64::MAX,
        (TaskSummaryOrder::NewestFirst, _) => cursor - 1,
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
