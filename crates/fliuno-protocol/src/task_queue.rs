//! A session's pending prompts and durable dispatch receipts.
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

pub const MAX_SESSION_PENDING_PROMPTS: usize = 16;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TaskQueueItemParams {
    pub session_id: String,
    pub queue_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum TaskQueueState {
    Pending,
    Started {
        #[serde(rename = "taskId")]
        task_id: String,
    },
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TaskQueueItem {
    pub queue_id: String,
    pub input_preview: String,
    pub plan_only: bool,
    pub status: TaskQueueState,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TaskQueue {
    pub session_id: String,
    pub pending: Vec<TaskQueueItem>,
    /// A task in progress, waiting for the user, or interrupted in this session.
    pub blocking_task_id: Option<String>,
    pub archived: bool,
}
