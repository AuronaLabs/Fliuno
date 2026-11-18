//! Session metadata updates preserve identity and row order.
use super::*;
use crate::task::validate_key;

pub(super) fn migrate_requests(db: &mut Connection) -> Result<()> {
    let tx = db.transaction()?;
    tx.execute_batch(
        "CREATE TABLE IF NOT EXISTS session_requests(
            workspace_id TEXT NOT NULL REFERENCES workspaces(id),
            request_key TEXT NOT NULL,
            session_id TEXT NOT NULL REFERENCES sessions(id),
            original_title TEXT,
            PRIMARY KEY(workspace_id,request_key)
        ) WITHOUT ROWID;
        PRAGMA user_version=17;",
    )?;
    tx.commit()?;
    Ok(())
}

pub(super) fn migrate_archive_index(db: &mut Connection) -> Result<()> {
    let tx = db.transaction()?;
    tx.execute_batch(
        "CREATE INDEX IF NOT EXISTS sessions_workspace_archive
         ON sessions(workspace_id, COALESCE(json_extract(data,'$.archived'),0));
         PRAGMA user_version=19;",
    )?;
    tx.commit()?;
    Ok(())
}

fn validate_session_title(title: &str) -> Result<&str> {
    if title.chars().take(MAX_SESSION_TITLE_CHARS + 1).count() > MAX_SESSION_TITLE_CHARS
        || title.trim().is_empty()
        || title.chars().any(char::is_control)
    {
        return Err(Error::Invalid(format!(
            "session title must have 1..{MAX_SESSION_TITLE_CHARS} characters and no control characters"
        )));
    }
    Ok(title.trim())
}

impl Kernel {
    pub fn create_session(&mut self, workspace_id: &str) -> Result<Session> {
        self.create_session_with_title(workspace_id, None)
    }

    pub fn create_session_with_title(
        &mut self,
        workspace_id: &str,
        title: Option<&str>,
    ) -> Result<Session> {
        self.create_session_with_request(workspace_id, title, None)
    }

    /// A retry compares the original input and returns the current session.
    pub fn create_session_with_request(
        &mut self,
        workspace_id: &str,
        title: Option<&str>,
        request_key: Option<&str>,
    ) -> Result<Session> {
        let title = title.map(validate_session_title).transpose()?;
        if let Some(key) = request_key {
            validate_key(key)?;
            let previous = self.db.query_row(
                "SELECT session_id,original_title FROM session_requests WHERE workspace_id=?1 AND request_key=?2",
                params![workspace_id, key],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?)),
            ).optional()?;
            if let Some((session_id, original_title)) = previous {
                if original_title.as_deref() != title {
                    return Err(Error::Conflict(
                        "clientRequestId was used with a different session title".into(),
                    ));
                }
                return self.get_session(&session_id);
            }
        }
        let exists: bool = self.db.query_row(
            "SELECT EXISTS(SELECT 1 FROM workspaces WHERE id=?1)",
            [workspace_id],
            |r| r.get(0),
        )?;
        if !exists {
            return Err(Error::NotFound("workspace".into()));
        }
        let session = Session {
            id: new_id("session"),
            workspace_id: workspace_id.into(),
            title: title.map(str::to_owned),
            archived: false,
        };
        let tx = self.db.transaction()?;
        tx.execute(
            "INSERT INTO sessions VALUES(?1,?2,?3)",
            params![
                session.id,
                session.workspace_id,
                serde_json::to_string(&session)?
            ],
        )?;
        if let Some(key) = request_key {
            tx.execute(
                "INSERT INTO session_requests(workspace_id,request_key,session_id,original_title) VALUES(?1,?2,?3,?4)",
                params![workspace_id, key, session.id, title],
            )?;
        }
        tx.commit()?;
        Ok(session)
    }

    pub fn get_session(&self, id: &str) -> Result<Session> {
        let data: String = self
            .db
            .query_row("SELECT data FROM sessions WHERE id=?1", [id], |row| {
                row.get(0)
            })
            .optional()?
            .ok_or_else(|| Error::NotFound("session".into()))?;
        Ok(serde_json::from_str(&data)?)
    }

    pub fn rename_session(&mut self, id: &str, title: &str) -> Result<Session> {
        let title = validate_session_title(title)?;
        let mut session = self.get_session(id)?;
        if session.title.as_deref() == Some(title) {
            return Ok(session);
        }
        session.title = Some(title.into());
        self.save_session(&session)?;
        Ok(session)
    }

    pub fn set_session_archived(&mut self, id: &str, archived: bool) -> Result<Session> {
        let mut session = self.get_session(id)?;
        if session.archived != archived {
            session.archived = archived;
            self.save_session(&session)?;
        }
        Ok(session)
    }

    fn save_session(&self, session: &Session) -> Result<()> {
        self.db.execute(
            "UPDATE sessions SET data=?2 WHERE id=?1",
            params![session.id, serde_json::to_string(session)?],
        )?;
        Ok(())
    }
}
