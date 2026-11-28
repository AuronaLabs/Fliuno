//! Keep workspace guide consent separate from user-authored instructions.
use super::*;
use crate::workspace_path::{is_reparse_point, open_workspace_root};
use cap_fs_ext::{FollowSymlinks, MetadataExt, OpenOptionsFollowExt};
use cap_std::fs::OpenOptions;
use rusqlite::{OptionalExtension, params};
use sha2::{Digest, Sha256};
use std::{
    io::{self, Read},
    path::Path,
};

pub(crate) fn migrate(db: &Connection) -> Result<()> {
    db.execute_batch(
        "BEGIN IMMEDIATE;
        CREATE TABLE IF NOT EXISTS workspace_project_guides(
            workspace_id TEXT PRIMARY KEY REFERENCES workspaces(id),
            revision INTEGER NOT NULL CHECK(revision > 0),
            enabled_sha256 TEXT CHECK(length(enabled_sha256) = 64)
        );
        PRAGMA user_version=77; COMMIT;",
    )?;
    Ok(())
}

impl Kernel {
    pub fn workspace_project_guide(&self, workspace_id: &str) -> Result<WorkspaceProjectGuide> {
        self.get_workspace(workspace_id)?;
        self.project_guide_state(workspace_id)
    }

    fn project_guide_state(&self, workspace_id: &str) -> Result<WorkspaceProjectGuide> {
        let (revision, enabled_sha256) = self.db.prepare_cached(
            "SELECT revision, enabled_sha256 FROM workspace_project_guides WHERE workspace_id=?1",
        )?.query_row([workspace_id], |row| Ok((row.get::<_, i64>(0)? as u64, row.get(1)?)))
            .optional()?.unwrap_or_default();
        Ok(WorkspaceProjectGuide {
            workspace_id: workspace_id.into(),
            revision,
            enabled_sha256,
        })
    }

    pub fn preview_project_guide(
        &self,
        workspace_id: &str,
    ) -> Result<WorkspaceProjectGuidePreview> {
        let workspace = self.get_workspace(workspace_id)?;
        let state = self.project_guide_state(workspace_id)?;
        let (guide, read_error) = match read_guide(Path::new(&workspace.root)) {
            Ok(guide) => (guide, None),
            Err(Error::Invalid(message) | Error::Conflict(message)) => (None, Some(message)),
            Err(error) => return Err(error),
        };
        Ok(WorkspaceProjectGuidePreview {
            state,
            guide,
            read_error,
        })
    }

    pub fn set_project_guide(
        &mut self,
        p: WorkspaceSetProjectGuideParams,
    ) -> Result<WorkspaceProjectGuide> {
        let workspace = self.get_workspace(&p.workspace_id)?;
        let current = self.project_guide_state(&p.workspace_id)?;
        if p.expected_revision != current.revision {
            return Err(Error::Conflict(
                "project guide setting changed; open a new preview".into(),
            ));
        }
        if let Some(expected) = &p.sha256 {
            if !valid_digest(expected) {
                return Err(Error::Invalid(
                    "sha256 must be a lowercase SHA-256 digest or null".into(),
                ));
            }
            current_guide(&workspace, expected)?;
        }
        if current.enabled_sha256 == p.sha256 {
            return Ok(current);
        }
        let revision = current
            .revision
            .checked_add(1)
            .filter(|n| *n < i64::MAX as u64)
            .ok_or_else(|| Error::Conflict("project guide revision limit reached".into()))?;
        self.db.execute(
            "INSERT INTO workspace_project_guides VALUES(?1,?2,?3)
             ON CONFLICT(workspace_id) DO UPDATE SET revision=excluded.revision,enabled_sha256=excluded.enabled_sha256",
            params![p.workspace_id, revision as i64, p.sha256],
        )?;
        Ok(WorkspaceProjectGuide {
            workspace_id: p.workspace_id,
            revision,
            enabled_sha256: p.sha256,
        })
    }

    pub(crate) fn capture_project_guide(
        &self,
        session_id: &str,
        revision: u64,
    ) -> Result<ProjectGuideSnapshot> {
        let session = self.get_session(session_id)?;
        let workspace = self.get_workspace(&session.workspace_id)?;
        let state = self.project_guide_state(&workspace.id)?;
        let digest = state
            .enabled_sha256
            .as_deref()
            .filter(|_| state.revision == revision)
            .ok_or_else(|| {
                Error::Conflict(
                    "project guide is disabled or the selected revision is outdated".into(),
                )
            })?;
        current_guide(&workspace, digest)
    }
}

fn valid_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || matches!(b, b'a'..=b'f'))
}

fn current_guide(workspace: &Workspace, digest: &str) -> Result<ProjectGuideSnapshot> {
    let guide = read_guide(Path::new(&workspace.root))?;
    guide.filter(|guide| guide.sha256 == digest).ok_or_else(|| {
        Error::Conflict(
            "AGENTS.md changed or disappeared; review it again or disable the project guide".into(),
        )
    })
}

fn read_guide(root: &Path) -> Result<Option<ProjectGuideSnapshot>> {
    let dir = open_workspace_root(root).map_err(|_| {
        Error::Conflict("cannot open the original project directory without following links".into())
    })?;
    let unreadable =
        || Error::Invalid("AGENTS.md must be a readable regular file without links".into());
    let metadata = match dir.symlink_metadata(PROJECT_GUIDE_PATH) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(unreadable()),
    };
    if !metadata.is_file() || metadata.nlink() != 1 || is_reparse_point(&metadata) {
        return Err(unreadable());
    }
    let mut options = OpenOptions::new();
    options.read(true).follow(FollowSymlinks::No);
    #[cfg(unix)]
    {
        use cap_fs_ext::OpenOptionsSyncExt;
        options.nonblock(true);
    }
    let file = dir
        .open_with(PROJECT_GUIDE_PATH, &options)
        .map_err(|_| unreadable())?;
    let metadata = file.metadata().map_err(|_| unreadable())?;
    if !metadata.is_file() || metadata.nlink() != 1 || is_reparse_point(&metadata) {
        return Err(unreadable());
    }
    let too_long = || {
        Error::Invalid(format!(
            "AGENTS.md exceeds {MAX_PROJECT_GUIDE_BYTES} UTF-8 bytes"
        ))
    };
    if metadata.len() > MAX_PROJECT_GUIDE_BYTES as u64 {
        return Err(too_long());
    }
    let mut bytes = Vec::new();
    file.take((MAX_PROJECT_GUIDE_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| unreadable())?;
    if bytes.len() > MAX_PROJECT_GUIDE_BYTES {
        return Err(too_long());
    }
    let text = String::from_utf8(bytes)
        .map_err(|_| Error::Invalid("AGENTS.md must contain UTF-8 text".into()))?;
    if text.trim().is_empty() {
        return Ok(None);
    }
    Ok(Some(ProjectGuideSnapshot {
        path: PROJECT_GUIDE_PATH.into(),
        sha256: format!("{:x}", Sha256::digest(text.as_bytes())),
        text,
    }))
}

#[cfg(test)]
mod tests;
