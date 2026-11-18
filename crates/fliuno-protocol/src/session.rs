//! Wire types for naming and retrieving a workspace session.
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

pub const MAX_SESSION_TITLE_CHARS: usize = 120;

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Session {
    pub id: String,
    pub workspace_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(min = 1, max = MAX_SESSION_TITLE_CHARS))]
    pub title: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SessionCreateParams {
    pub workspace_id: String,
    #[serde(default)]
    #[schemars(length(min = 1, max = MAX_SESSION_TITLE_CHARS))]
    pub title: Option<String>,
    #[serde(default)]
    #[schemars(length(min = 1, max = 128), regex(pattern = "^[!-~]+$"))]
    pub client_request_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SessionIdParams {
    pub session_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SessionRenameParams {
    pub session_id: String,
    #[schemars(length(min = 1, max = MAX_SESSION_TITLE_CHARS))]
    pub title: String,
}
