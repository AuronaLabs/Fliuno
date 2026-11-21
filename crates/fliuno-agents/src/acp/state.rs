//! Scope state to a registered executable and workspace. Task-addressed
//! snapshots avoid changing the predecessor until the new run completes.
use super::{AcpAgentConfig, AcpBackend, AgentRequest, Isolation};
use fliuno_core::authority_digest;
use fliuno_sandbox::PrivateState;
use serde_json::json;
use std::{fs, io, path::PathBuf};

pub(super) fn resolve_root(config: &AcpAgentConfig) -> Result<Option<PathBuf>, String> {
    let Some(root) = config.session_state_directory.as_ref() else {
        return Ok(None);
    };
    if !matches!(config.isolation, Isolation::NativeScratch)
        || !cfg!(any(windows, target_os = "linux"))
        || !root.is_absolute()
        || !root.is_dir()
    {
        return Err(
            "ACP sessionStateDirectory requires native_scratch and an absolute existing directory"
                .into(),
        );
    }
    root.canonicalize()
        .map(Some)
        .map_err(|_| "cannot resolve ACP session state directory".into())
}

impl AcpBackend {
    pub(super) fn private_state(&self, request: &AgentRequest) -> io::Result<Option<PrivateState>> {
        let Some(root) = &self.session_state_directory else {
            return Ok(None);
        };
        if root.starts_with(fs::canonicalize(&request.workspace.root)?) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "state directory is inside the workspace",
            ));
        }
        let scope = root.join(authority_digest(
            "acp.state.scope.v1",
            &json!([self.authority, request.workspace.id,]),
        ));
        match fliuno_sandbox::create_private_directory(&scope) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
        if !fs::symlink_metadata(&scope)?.is_dir() || scope.canonicalize()? != scope {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid state scope",
            ));
        }
        let slot =
            |task_id: &str| scope.join(authority_digest("acp.state.task.v1", &json!(task_id)));
        let previous = request
            .agent_session_continuation
            .as_ref()
            .map(|prior| slot(&prior.source_task_id));
        if previous.as_ref().is_some_and(|path| !path.is_dir()) {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "missing ACP checkpoint",
            ));
        }
        Ok(Some(PrivateState {
            previous,
            next: slot(&request.task_id),
        }))
    }
}
