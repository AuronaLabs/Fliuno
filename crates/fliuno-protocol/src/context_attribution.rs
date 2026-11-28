use crate::{
    ContextMemoryMatch, ContextOptions, ContextSummarySource, Event, ModelBudget,
    ModelBudgetInputBytes, ProjectGuideAttribution, ReplyIdentity,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkspaceInstructions {
    pub workspace_id: String,
    pub revision: u64,
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkspaceSetInstructionsParams {
    pub workspace_id: String,
    pub expected_revision: u64,
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ContextMemoryAttribution {
    pub id: String,
    pub revision: u64,
    pub matched_by: Option<ContextMemoryMatch>,
    pub source_task_id: Option<String>,
    pub normalized_bytes: u64,
    pub omitted_for_budget: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TaskContextAttribution {
    pub task_id: String,
    pub model_steps: u32,
    pub history: Vec<ContextSummarySource>,
    pub omitted_history: u64,
    pub summarized_history: Vec<ContextSummarySource>,
    pub memories: Vec<ContextMemoryAttribution>,
    pub instructions_revision: Option<u64>,
    pub instructions_bytes: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_guide: Option<crate::ProjectGuideAttribution>,
    pub normalized_input_bytes: Option<ModelBudgetInputBytes>,
    pub latest_preflight: Option<Event>,
    pub latest_budget_rejection: Option<Event>,
}

/// Read-only preview of a text draft and the active model's available context.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DraftContextParams {
    pub session_id: String,
    /// Empty text is valid for inspecting the baseline without sending anything.
    #[serde(default)]
    pub input: String,
    #[serde(default)]
    pub options: ContextOptions,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum DraftMemoryMode {
    Off,
    Explicit,
    KeywordPreview,
}

/// Snapshot of candidate input fragments, not a token count or a send receipt.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DraftContextReport {
    pub through_sequence: u64,
    pub model: Option<ReplyIdentity>,
    pub budget: Option<ModelBudget>,
    pub history_turns: u32,
    pub omitted_turns: u64,
    pub memory_mode: DraftMemoryMode,
    pub memory_count: u32,
    pub tool_count: u32,
    pub instructions_bytes: u64,
    pub project_guide: Option<ProjectGuideAttribution>,
    pub normalized_input_bytes: ModelBudgetInputBytes,
}
