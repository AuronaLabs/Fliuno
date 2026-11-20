//! Compact invalidations for a host that keeps a cache of task summaries.
use super::TaskSummary;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

pub const MAX_TASK_CHANGE_EVENTS: u32 = 1000;
pub const MAX_TASK_SUMMARY_CHANGE_EVENTS: u32 = 64;

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

/// The same replay window as `TaskChangesParams`, with a smaller response budget.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TaskSummaryChangesParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
    #[serde(default)]
    pub after: u64,
    /// Inclusive end of the event window. Omit to capture the current watermark.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub through: Option<u64>,
    /// Maximum events to examine before deduplication. Latest revision replay
    /// uses this as a result limit and scans up to MAX_TASK_CHANGE_EVENTS.
    #[serde(default = "default_summary_change_limit")]
    #[schemars(range(min = 1, max = MAX_TASK_SUMMARY_CHANGE_EVENTS))]
    pub limit: u32,
}

const fn default_summary_change_limit() -> u32 {
    MAX_TASK_SUMMARY_CHANGE_EVENTS
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TaskSummaryChange {
    /// Last event for this task in the scanned page, not its current revision.
    pub sequence: u64,
    /// Current summary at `summaryThroughSequence`, possibly newer than `sequence`.
    pub summary: TaskSummary,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TaskSummaryChangesPage {
    /// One summary per changed task, ordered by last event in this page.
    pub changes: Vec<TaskSummaryChange>,
    pub next_after: u64,
    pub through_sequence: u64,
    pub has_more: bool,
    /// Current global event watermark from the read snapshot. Never a replay cursor.
    pub summary_through_sequence: u64,
}
