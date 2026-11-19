//! Read-only approval inbox, decision receipts, and run bindings.
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

/// The event and expiry-time boundaries of a read-only inbox snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ApprovalInboxSnapshot {
    pub through_sequence: u64,
    pub captured_at_unix_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ApprovalInboxListParams {
    pub page: ApprovalListParams,
    /// Reuse the first response's snapshot when continuing a page.
    #[serde(default)]
    pub snapshot: Option<ApprovalInboxSnapshot>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ApprovalInboxPage {
    pub snapshot: ApprovalInboxSnapshot,
    pub page: super::ObjectPage<ApprovalSummary>,
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

/// Upper bounds for a receipt batch and each of its opaque identifiers.
pub const MAX_APPROVAL_DECISION_BATCH: usize = 64;
pub const MAX_APPROVAL_DECISION_ID_BYTES: usize = 128;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ApprovalDecisionKey {
    #[schemars(length(min = 1, max = MAX_APPROVAL_DECISION_ID_BYTES), regex(pattern = "^[!-~]+$"))]
    pub task_id: String,
    #[schemars(length(min = 1, max = MAX_APPROVAL_DECISION_ID_BYTES), regex(pattern = "^[!-~]+$"))]
    pub approval_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ApprovalDecisionBatchParams {
    #[schemars(length(min = 1, max = MAX_APPROVAL_DECISION_BATCH))]
    pub approvals: Vec<ApprovalDecisionKey>,
}

/// A committed decision, without the original approval request or tool data.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ApprovalDecisionReceipt {
    pub task_id: String,
    pub approval_id: String,
    pub decision: super::ApprovalDecision,
    pub sequence: u64,
    /// Present for an Agent decision. This records intent, not peer delivery.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ApprovalDecisionBatch {
    /// One position per requested key. Pending, closed without a decision,
    /// and unknown keys have no receipt.
    #[schemars(length(min = 1, max = MAX_APPROVAL_DECISION_BATCH))]
    pub items: Vec<Option<ApprovalDecisionReceipt>>,
    /// Global event watermark from the same SQLite read snapshot.
    pub through_sequence: u64,
}
