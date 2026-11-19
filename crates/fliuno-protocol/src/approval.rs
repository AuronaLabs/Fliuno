//! Read-only summaries for the pending approval inbox.
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ApprovalResumeParams {
    pub session_id: String,
    pub parent_request_id: String,
    pub client_request_id: String,
    pub approval_id: String,
    pub decision: super::ApprovalDecision,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ApprovalRun {
    pub task: super::TaskSummary,
    pub start_sequence: Option<u64>,
}

/// Inclusive bounds of one segment of a task's approval lifecycle.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ApprovalRunWindow {
    pub task_id: String,
    pub first_sequence: u64,
    /// `None` while no event has sealed the run.
    pub last_sequence: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ApprovalRunPendingParams {
    pub session_id: String,
    pub client_request_id: String,
    pub approval_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ApprovalListParams {
    #[serde(default)]
    pub workspace_id: Option<String>,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub after: u64,
    #[serde(default = "super::default_limit")]
    #[schemars(range(min = 1, max = 100))]
    pub limit: u32,
    /// Compact items array byte target. A larger first approval still advances the cursor.
    #[serde(default)]
    pub target_bytes: Option<u32>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalKind {
    ToolCall,
    AgentPermission,
    HostTool,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ApprovalSummary {
    pub id: String,
    pub task_id: String,
    pub session_id: String,
    pub kind: ApprovalKind,
    pub allow_once_available: bool,
    pub expires_at_unix_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ApprovalGetParams {
    pub task_id: String,
    pub approval_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ApprovalDetails {
    ToolCall { approval: super::Approval },
    AgentPermission { approval: super::AgentApproval },
    HostTool { approval: super::AgentApproval },
}
