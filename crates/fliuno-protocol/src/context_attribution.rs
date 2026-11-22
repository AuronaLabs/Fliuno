use crate::{ContextMemoryMatch, ContextSummarySource, Event, ModelBudgetInputBytes};
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
    pub normalized_input_bytes: Option<ModelBudgetInputBytes>,
    pub latest_preflight: Option<Event>,
    pub latest_budget_rejection: Option<Event>,
}
