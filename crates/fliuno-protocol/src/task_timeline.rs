//! A compact, paginated index into one task's durable event log.
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

pub const TASK_TIMELINE_PAGE_SIZE: u32 = 32;

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TaskTimelineParams {
    pub task_id: String,
    #[serde(default)]
    pub after: u64,
    #[serde(default)]
    pub through_sequence: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TaskTimelineEntry {
    pub sequence: u64,
    pub kind: String,
    pub payload_bytes: u64,
    pub approval_id: Option<String>,
    pub tool_name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TaskTimelinePage {
    pub task_id: String,
    pub through_sequence: u64,
    pub entries: Vec<TaskTimelineEntry>,
    pub next_after: Option<u64>,
}
