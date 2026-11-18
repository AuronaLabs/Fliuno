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
        let exists: bool = self.db.query_row(
            "SELECT EXISTS(SELECT 1 FROM workspaces WHERE id=?1)",
            [workspace],
            |r| r.get(0),
        )?;
        if !exists {
            return Err(Error::NotFound("workspace".into()));
        }
        self.object_page("SELECT rowid,data FROM sessions WHERE workspace_id=?1 AND rowid>?2 ORDER BY rowid LIMIT ?3",Some(workspace),after,limit)
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

    pub fn list_task_summaries(
        &self,
        session: &str,
        after: u64,
        limit: u32,
    ) -> Result<ObjectPage<TaskSummary>> {
        self.require_session(session)?;
        // Extract a small page from the existing records without loading full
        // histories, tool arguments or artifacts into Rust. The cursor is the
        // same row position used for the full task list.
        self.object_page(
            r#"SELECT rowid,json_object(
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
             ) FROM tasks WHERE session_id=?1 AND rowid>?2 ORDER BY rowid LIMIT ?3"#,
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
