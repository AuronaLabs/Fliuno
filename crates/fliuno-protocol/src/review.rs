use crate::{AgentTaskState, ArtifactDigest, ArtifactGetParams};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TaskReconcileParams {
    pub task_id: String,
    pub state: AgentTaskState,
    pub operator: String,
    pub evidence: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub check_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TaskReconciliation {
    pub run_id: String,
    pub recorded_at_ms: u64,
    pub review: TaskReconcileParams,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ArtifactLinkRevisionParams {
    pub previous: ArtifactGetParams,
    pub current: ArtifactGetParams,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ArtifactRevision {
    pub previous: ArtifactGetParams,
    pub current: ArtifactGetParams,
    pub previous_digest: ArtifactDigest,
    pub current_digest: ArtifactDigest,
}
