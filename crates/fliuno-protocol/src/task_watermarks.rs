//! Bounded lookups for hosts following multiple tasks on one connection.
use super::MAX_TASK_SUMMARY_ID_BYTES;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

pub const MAX_TASK_WATERMARKS: usize = 64;

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TaskWatermarksParams {
    #[schemars(length(min = 1, max = MAX_TASK_WATERMARKS))]
    #[schemars(inner(length(min = 1, max = MAX_TASK_SUMMARY_ID_BYTES), regex(pattern = "^[!-~]+$")))]
    pub task_ids: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TaskWatermarks {
    /// Latest global event sequence for each requested task, in input order.
    /// A task with no durable events has a null entry.
    #[schemars(length(max = MAX_TASK_WATERMARKS))]
    pub through_sequences: Vec<Option<u64>>,
}
