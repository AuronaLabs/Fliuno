use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Only tools selected by the local host can participate in read rules.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PolicyReadTool {
    pub name: String,
    pub authority: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PolicyReadRuleCreateParams {
    pub workspace_id: String,
    /// Omit for a workspace rule. A session must belong to this workspace.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    pub tool_name: String,
    pub expected_authority: String,
    /// The exact tool argument object the user reviewed.
    pub arguments: Value,
    pub expires_at_ms: i64,
    pub client_request_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PolicyReadRule {
    pub id: String,
    pub workspace_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    pub tool_name: String,
    pub authority: String,
    pub arguments: Value,
    pub expires_at_ms: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revoked_at_ms: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PolicyReadRuleRevokeParams {
    pub workspace_id: String,
    pub rule_id: String,
}
