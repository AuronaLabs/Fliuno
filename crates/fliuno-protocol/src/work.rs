//! A coherent starting point for a host's task and approval views.
use super::{ApprovalInboxPage, ApprovalListParams};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkSnapshotParams {
    #[serde(default)]
    pub workspace_id: Option<String>,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default = "super::default_limit")]
    #[schemars(range(min = 1, max = 100))]
    pub approval_limit: u32,
    #[serde(default)]
    pub approval_target_bytes: Option<u32>,
}

impl WorkSnapshotParams {
    /// Translate the scope and limits to a first-page inbox request.
    pub fn approval_page(&self) -> ApprovalListParams {
        ApprovalListParams {
            workspace_id: self.workspace_id.clone(),
            session_id: self.session_id.clone(),
            after: 0,
            limit: self.approval_limit,
            target_bytes: self.approval_target_bytes,
        }
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TaskStatusCounts {
    pub running: u64,
    pub waiting_approval: u64,
    pub interrupted: u64,
    pub completed: u64,
    pub cancelled: u64,
    pub failed: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkSnapshot {
    pub task_counts: TaskStatusCounts,
    /// Its stamp applies to the entire response. The inbox only includes
    /// approvals that had not expired when the snapshot was captured.
    pub approvals: ApprovalInboxPage,
}
