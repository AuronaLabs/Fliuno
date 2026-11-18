//! Protocol-neutral remote identities and inert artifact import. No network,
//! URL fetching, workspace writes, or peer-controlled local artifact IDs.
use super::*;
use std::collections::BTreeSet;

mod check;
mod continuation;
pub use check::{CompletedRemoteTaskCheck, PreparedRemoteTaskCheck};

pub struct IncomingAgentArtifact {
    pub source_id: String,
    pub title: String,
    pub media_type: String,
    pub content: String,
}
fn valid_id(value: &str) -> bool {
    !value.is_empty() && value.len() <= 256 && !value.chars().any(char::is_control)
}
impl Kernel {
    pub(crate) fn observe_remote_task(
        &mut self,
        task_id: &str,
        run_id: &str,
        remote: AgentRemoteTask,
    ) -> Result<()> {
        let mut task = self.get_task(task_id)?;
        if !matches!(
            task.status,
            TaskStatus::Running | TaskStatus::WaitingApproval | TaskStatus::Cancelled
        ) {
            return Err(Error::Conflict(
                "remote task observation is no longer active".into(),
            ));
        }
        let run = task
            .agent_run
            .as_mut()
            .filter(|r| r.id == run_id && !r.finished)
            .ok_or_else(|| Error::Conflict("remote task belongs to an inactive run".into()))?;
        if !valid_id(&remote.id) || remote.context_id.as_ref().is_some_and(|s| !valid_id(s)) {
            return Err(Error::Invalid("invalid remote task identity".into()));
        }
        if let Some(previous) = &run.remote_task {
            if previous.id != remote.id
                || previous.context_id != remote.context_id
                || previous.state.is_terminal() && previous.state != remote.state
            {
                return Err(Error::Conflict(
                    "remote task identity or terminal state changed".into(),
                ));
            }
            if previous == &remote {
                return Ok(());
            }
        }
        let bytes = json_size(&remote)? as u64;
        if run.update_count >= 4096 || run.recorded_bytes + bytes > agent::MAX_RECORDED_BYTES {
            return Err(Error::Conflict(
                "remote task evidence budget exceeded".into(),
            ));
        }
        run.remote_task = Some(remote.clone());
        run.update_count += 1;
        run.recorded_bytes += bytes;
        self.commit(
            &task,
            &[EventKind::AgentRemoteTaskUpdated {
                run_id: run_id.into(),
                task: remote,
            }],
            &[],
        )
    }

    /// Import one final snapshot atomically. Partial poll artifacts stay remote;
    /// source/local identity links, artifacts, budget and events commit together.
    pub(crate) fn observe_agent_artifacts(
        &mut self,
        task_id: &str,
        run_id: &str,
        incoming: Vec<IncomingAgentArtifact>,
    ) -> Result<()> {
        let mut task = self.get_task(task_id)?;
        if task.status != TaskStatus::Running {
            return Err(Error::Conflict(
                "agent artifact import is no longer active".into(),
            ));
        }
        let run = task
            .agent_run
            .as_mut()
            .filter(|r| r.id == run_id && !r.finished)
            .ok_or_else(|| Error::Conflict("artifact import belongs to an inactive run".into()))?;
        if !run.artifacts.is_empty() || incoming.len() > 64 {
            return Err(Error::Conflict(
                "duplicate or oversized agent artifact import".into(),
            ));
        }
        let mut sources = BTreeSet::new();
        let mut artifacts = Vec::new();
        let mut links = Vec::new();
        let mut bytes = 0_u64;
        for source in incoming {
            if !valid_id(&source.source_id)
                || !sources.insert(source.source_id.clone())
                || source.title.trim().is_empty()
                || source.title.len() > 256
                || source.media_type.is_empty()
                || source.media_type.len() > 128
                || source.media_type.chars().any(char::is_control)
                || source.content.len() > 262_144
            {
                return Err(Error::Invalid(
                    "invalid agent artifact or import budget exceeded".into(),
                ));
            }
            let artifact = Artifact {
                id: new_id("artifact"),
                task_id: task_id.into(),
                title: source.title,
                media_type: source.media_type,
                content: source.content,
            };
            links.push(AgentArtifactLink {
                source_id: source.source_id,
                artifact_id: artifact.id.clone(),
            });
            bytes += json_size(&artifact)? as u64;
            artifacts.push(artifact);
        }
        if run.recorded_bytes + bytes > agent::MAX_RECORDED_BYTES {
            return Err(Error::Conflict(
                "agent artifact aggregate budget exceeded".into(),
            ));
        }
        run.recorded_bytes += bytes;
        run.artifacts = links;
        let events = artifacts
            .iter()
            .map(|a| EventKind::ArtifactCreated {
                artifact_id: a.id.clone(),
            })
            .collect::<Vec<_>>();
        self.commit(&task, &events, &artifacts)
    }
}
