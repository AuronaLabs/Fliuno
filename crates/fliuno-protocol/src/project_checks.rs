//! Read-only catalog of host-registered project checks.
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkspaceProjectChecks {
    pub workspace_id: String,
    pub checks: Vec<ProjectCheckSummary>,
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProjectCheckSummary {
    pub tool_name: String,
    pub label: String,
    pub description: String,
    pub inputs: Vec<ProjectCheckInput>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProjectCheckInput {
    pub path: String,
    pub status: ProjectCheckInputStatus,
}

/// Advisory metadata only: execution validates bytes and freshly approved hashes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum ProjectCheckInputStatus {
    Present,
    Missing,
    Unavailable,
    Unsupported,
}
