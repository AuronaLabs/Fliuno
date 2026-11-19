//! Cancel a bounded selection in an explicit session or workspace.
use crate::TaskSummary;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

pub const MAX_TASK_CANCEL_BATCH: usize = 32;
pub const MAX_TASK_CANCEL_ID_BYTES: usize = 128;

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(
    tag = "kind",
    rename_all = "snake_case",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum TaskCancelScope {
    Session { session_id: String },
    Workspace { workspace_id: String },
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TaskCancelBatchParams {
    pub scope: TaskCancelScope,
    #[schemars(length(min = 1, max = MAX_TASK_CANCEL_BATCH))]
    #[schemars(inner(length(min = 1, max = MAX_TASK_CANCEL_ID_BYTES), regex(pattern = "^[!-~]+$")))]
    pub task_ids: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TaskCancelBatchItem {
    pub task: TaskSummary,
    /// True only when this request committed a new cancellation for this task.
    pub changed: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TaskCancelBatch {
    /// Summaries in request order, including tasks already in a terminal state.
    pub items: Vec<TaskCancelBatchItem>,
    /// Global event watermark after the batch's durable transitions.
    pub through_sequence: u64,
}
