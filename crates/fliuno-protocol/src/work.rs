//! A coherent starting point for a host's task and approval views.
use super::{ApprovalInboxPage, ApprovalListParams, TaskSummary};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

pub const MAX_WORK_SESSION_COUNT_BATCH: usize = 64;
pub const MAX_WORK_SESSION_COUNT_ID_BYTES: usize = 128;

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkSessionCountsParams {
    #[schemars(length(min = 1, max = MAX_WORK_SESSION_COUNT_ID_BYTES), regex(pattern = "^[!-~]+$"))]
    pub workspace_id: String,
    #[schemars(length(min = 1, max = MAX_WORK_SESSION_COUNT_BATCH))]
    #[schemars(inner(length(min = 1, max = MAX_WORK_SESSION_COUNT_ID_BYTES), regex(pattern = "^[!-~]+$")))]
    pub session_ids: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkSessionCount {
    pub session_id: String,
    pub task_counts: TaskStatusCounts,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkSessionCounts {
    /// One position per input. A missing or foreign session is null.
    #[schemars(length(min = 1, max = MAX_WORK_SESSION_COUNT_BATCH))]
    pub items: Vec<Option<WorkSessionCount>>,
    /// The global event watermark in the same read view as the counts.
    pub through_sequence: u64,
}

/// Bound the number of compact summaries loaded for one work snapshot.
pub const MAX_WORK_RECENT_TASKS: u32 = 50;

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
    /// Request a preview in reverse creation order within the snapshot scope.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 1, max = MAX_WORK_RECENT_TASKS))]
    pub recent_task_limit: Option<u32>,
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
    #[serde(default)]
    pub waiting_input: u64,
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
    /// Compact tasks from the same read view. Omitted unless requested.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(max = MAX_WORK_RECENT_TASKS))]
    pub recent_tasks: Option<Vec<TaskSummary>>,
}
