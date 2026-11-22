use crate::TaskStatus;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

pub const MAX_TASK_STEERING_BYTES: usize = 4096;
pub const MAX_TASK_STEERING_REQUESTS: u32 = 8;

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TaskSteerParams {
    pub task_id: String,
    pub client_request_id: String,
    pub expected_sequence: u64,
    pub instruction: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TaskSteeringReceipt {
    pub client_request_id: String,
    pub accepted_sequence: u64,
    /// Step whose durable model input contains the instruction.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attached_step: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TaskSteeringState {
    pub status: TaskStatus,
    pub through_sequence: u64,
    pub receipts: Vec<TaskSteeringReceipt>,
}
