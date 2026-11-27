//! Wire types for organizing and retrieving workspace sessions.
use super::default_limit;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

pub const MAX_SESSION_TITLE_CHARS: usize = 120;
pub const MAX_SESSION_ARCHIVE_BATCH: usize = 32;
pub const MAX_SESSION_FORK_TURNS: u32 = 64;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SessionForkOrigin {
    pub session_id: String,
    pub through_sequence: u64,
    pub inherited_turns: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SessionForkParams {
    pub session_id: String,
    pub client_request_id: String,
    pub title: Option<String>,
    /// An inclusive completion boundary, resolved against the source session.
    pub through_task_id: Option<String>,
    /// Mutually exclusive with throughTaskId. Omit both for the current watermark.
    pub through_sequence: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Session {
    pub id: String,
    pub workspace_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(min = 1, max = MAX_SESSION_TITLE_CHARS))]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub archived: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fork_origin: Option<SessionForkOrigin>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SessionListParams {
    pub workspace_id: String,
    #[serde(default)]
    pub archived: Option<bool>,
    #[serde(default)]
    pub after: u64,
    #[serde(default = "default_limit")]
    pub limit: u32,
    /// Omit for the original ascending catalog order.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub order: Option<SessionListOrder>,
    /// Literal title or session ID substring. ASCII matching is case insensitive.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(min = 1, max = MAX_SESSION_TITLE_CHARS))]
    pub query: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SessionListOrder {
    #[default]
    OldestFirst,
    NewestFirst,
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

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SessionSetArchivedParams {
    pub session_id: String,
    pub archived: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SessionArchiveBatchParams {
    #[schemars(length(min = 1, max = MAX_SESSION_ARCHIVE_BATCH))]
    pub session_ids: Vec<String>,
    pub archived: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SessionArchiveBatch {
    /// Sessions follow request order; the batch is committed atomically.
    pub sessions: Vec<Session>,
}
