//! Bounded point reads for updating a host's task list.
use crate::{MAX_CLIENT_REQUEST_ID_BYTES, TaskResumeStatus, TaskSummary};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

pub const MAX_TASK_SUMMARY_BATCH: usize = 64;
pub const MAX_TASK_SUMMARY_ID_BYTES: usize = 128;

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
/// Request order and optional scope for bounded task point reads.
pub struct TaskSummaryBatchParams {
    #[schemars(length(min = 1, max = MAX_TASK_SUMMARY_BATCH))]
    #[schemars(inner(length(min = 1, max = MAX_TASK_SUMMARY_ID_BYTES), regex(pattern = "^[!-~]+$")))]
    pub task_ids: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TaskRequestSummaryBatchParams {
    pub session_id: String,
    /// The keys originally used as clientRequestId in the selected session.
    #[schemars(length(min = 1, max = MAX_TASK_SUMMARY_BATCH))]
    #[schemars(inner(length(min = 1, max = MAX_CLIENT_REQUEST_ID_BYTES), regex(pattern = "^[!-~]+$")))]
    pub client_request_ids: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TaskSummaryBatch {
    /// One entry per requested ID or key, including duplicates. Missing and out-of-scope rows are null.
    pub items: Vec<Option<TaskSummary>>,
    /// Global event watermark read from the same database snapshot as the summaries.
    pub through_sequence: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TaskResumeStatusBatch {
    /// One entry per task ID. Missing and out-of-scope rows retain their position.
    pub items: Vec<Option<TaskResumeStatus>>,
    /// Global event watermark from the same read as the task state.
    pub through_sequence: u64,
}
