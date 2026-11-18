//! Session names are display metadata. Updates preserve session identity and row order.
use super::*;

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
        let title = title.map(validate_session_title).transpose()?;
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
        };
        self.db.execute(
            "INSERT INTO sessions VALUES(?1,?2,?3)",
            params![
                session.id,
                session.workspace_id,
                serde_json::to_string(&session)?
            ],
        )?;
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
        self.db.execute(
            "UPDATE sessions SET data=?2 WHERE id=?1",
            params![id, serde_json::to_string(&session)?],
        )?;
        Ok(session)
    }
}
