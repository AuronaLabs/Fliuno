//! Compact invalidations for a host that keeps a cache of task summaries.
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

pub const MAX_TASK_CHANGE_EVENTS: u32 = 1000;

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TaskChangesParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
    #[serde(default)]
    pub after: u64,
    /// Inclusive window end. If omitted, capture the current global watermark.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub through: Option<u64>,
    /// Maximum matching events to examine, before deduplicating task IDs.
    #[serde(default = "super::default_limit")]
    #[schemars(range(min = 1, max = MAX_TASK_CHANGE_EVENTS))]
    pub limit: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TaskChange {
    pub task_id: String,
    /// Last event for this task in the scanned page, not its current revision.
    pub sequence: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TaskChangesPage {
    /// Unique task IDs in ascending order of their last event in this page.
    pub changes: Vec<TaskChange>,
    pub next_after: u64,
    pub through_sequence: u64,
    pub has_more: bool,
}
