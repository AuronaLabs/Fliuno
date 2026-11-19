//! User-assigned workspace titles do not replace the canonical root or identity.
use super::*;
use crate::query::page_range;
use rusqlite::Row;

pub(super) fn migrate(db: &mut Connection) -> Result<()> {
    let tx = db.transaction()?;
    tx.execute_batch(
        "CREATE TABLE IF NOT EXISTS workspace_titles(
            workspace_id TEXT PRIMARY KEY REFERENCES workspaces(id),
            title TEXT NOT NULL
        ) WITHOUT ROWID;
        PRAGMA user_version=35;",
    )?;
    tx.commit()?;
    Ok(())
}

fn profile_row(row: &Row<'_>) -> rusqlite::Result<WorkspaceProfile> {
    Ok(WorkspaceProfile {
        workspace: Workspace {
            id: row.get(0)?,
            root: row.get(1)?,
        },
        title: row.get(2)?,
    })
}

impl Kernel {
    pub fn get_workspace_profile(&self, id: &str) -> Result<WorkspaceProfile> {
        self.db
            .prepare_cached(
                "SELECT w.id,w.root,t.title FROM workspaces w
            LEFT JOIN workspace_titles t ON t.workspace_id=w.id WHERE w.id=?1",
            )?
            .query_row([id], profile_row)
            .optional()?
            .ok_or_else(|| Error::NotFound("workspace".into()))
    }

    pub fn list_workspace_profiles(
        &self,
        after: u64,
        limit: u32,
    ) -> Result<ObjectPage<WorkspaceProfile>> {
        let (cursor, fetch) = page_range(after, limit)?;
        let mut statement = self.db.prepare_cached(
            "SELECT w.id,w.root,t.title,w.rowid FROM workspaces w
            LEFT JOIN workspace_titles t ON t.workspace_id=w.id
            WHERE w.rowid>?1 ORDER BY w.rowid LIMIT ?2",
        )?;
        let mut rows = statement.query(params![cursor, fetch])?;
        let mut page = ObjectPage {
            items: Vec::with_capacity(limit as usize),
            next_after: after,
            has_more: false,
        };
        while let Some(row) = rows.next()? {
            if page.items.len() == limit as usize {
                page.has_more = true;
                break;
            }
            page.items.push(profile_row(row)?);
            page.next_after = row.get::<_, i64>(3)? as u64;
        }
        Ok(page)
    }

    pub fn set_workspace_title(&mut self, id: &str, title: &str) -> Result<WorkspaceProfile> {
        let title = if title.is_empty() {
            None
        } else {
            let trimmed = title.trim();
            if trimmed.is_empty()
                || title.chars().take(MAX_WORKSPACE_TITLE_CHARS + 1).count()
                    > MAX_WORKSPACE_TITLE_CHARS
                || title.chars().any(char::is_control)
            {
                return Err(Error::Invalid(format!(
                    "workspace title must have 1..{MAX_WORKSPACE_TITLE_CHARS} characters and no control characters"
                )));
            }
            Some(trimmed)
        };
        let mut profile = self.get_workspace_profile(id)?;
        if profile.title.as_deref() == title {
            return Ok(profile);
        }
        if let Some(title) = title {
            self.db.execute(
                "INSERT INTO workspace_titles(workspace_id,title) VALUES(?1,?2)
                ON CONFLICT(workspace_id) DO UPDATE SET title=excluded.title",
                params![id, title],
            )?;
        } else {
            self.db
                .execute("DELETE FROM workspace_titles WHERE workspace_id=?1", [id])?;
        }
        profile.title = title.map(str::to_owned);
        Ok(profile)
    }
}
