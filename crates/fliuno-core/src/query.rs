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
        let mut statement = self.db.prepare(sql)?;
        let rows = statement.query_map(params![scope, after as i64, limit + 1], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
        })?;
        let mut items = Vec::new();
        let mut next_after = after;
        let mut has_more = false;
        for row in rows {
            let (sequence, data) = row?;
            if items.len() == limit as usize {
                has_more = true;
                break;
            }
            items.push(serde_json::from_str(&data)?);
            next_after = sequence as u64;
        }
        Ok(ObjectPage {
            items,
            next_after,
            has_more,
        })
    }
}
