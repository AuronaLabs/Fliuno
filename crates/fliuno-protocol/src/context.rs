//! Frozen, provider-neutral records of earlier completed work. These values
//! describe past effects; they never carry permissions or executable requests.
use crate::{AgentHostTool, TaskBackend, ToolCall};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ContextMode {
    #[default]
    Session,
    Isolated,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ContextSelection {
    #[default]
    Recent,
    FitRecent,
    /// Fit recent records whose conversation matches an explicit query.
    MatchRecent,
}

impl ContextSelection {
    fn is_recent(&self) -> bool {
        *self == Self::Recent
    }
}

/// Controls which parts of a completed record count toward the context budget.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ContextDetail {
    #[default]
    Full,
    /// Keep user and assistant messages and artifact references.
    Conversation,
}

impl ContextDetail {
    fn is_full(&self) -> bool {
        *self == Self::Full
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ContextOptions {
    #[serde(default)]
    pub mode: ContextMode,
    #[serde(default = "default_turns")]
    pub max_turns: u32,
    #[serde(default = "default_bytes")]
    pub max_bytes: u32,
    /// FitRecent may skip whole records within a bounded recent window.
    #[serde(default, skip_serializing_if = "ContextSelection::is_recent")]
    pub selection: ContextSelection,
    #[serde(default, skip_serializing_if = "ContextDetail::is_full")]
    pub detail: ContextDetail,
    /// Explicit completed tasks from this session; None keeps recent history.
    /// The kernel sorts IDs for stable request identity, then orders records by
    /// their completion sequence. Selected records are never silently omitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_ids: Option<Vec<String>>,
    /// Required in match_recent mode; searches only user and assistant text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub history_query: Option<ContextHistoryQuery>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ContextHistoryQuery {
    pub query: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<SessionHistoryRole>,
}

fn default_turns() -> u32 {
    16
}
fn default_bytes() -> u32 {
    128 * 1024
}

impl Default for ContextOptions {
    fn default() -> Self {
        Self {
            mode: ContextMode::Session,
            max_turns: default_turns(),
            max_bytes: default_bytes(),
            selection: ContextSelection::Recent,
            detail: ContextDetail::Full,
            task_ids: None,
            history_query: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(
    tag = "role",
    rename_all = "snake_case",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum ContextMessage {
    User { text: String },
    Assistant { text: String },
    ToolCall { call: ToolCall },
    ToolResult { call_id: String, output: Value },
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ContextArtifact {
    pub id: String,
    pub title: String,
    pub media_type: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ContextTurn {
    pub task_id: String,
    pub completion_sequence: u64,
    pub backend: TaskBackend,
    pub messages: Vec<ContextMessage>,
    pub artifacts: Vec<ContextArtifact>,
    pub host_tools: Vec<AgentHostTool>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TaskContext {
    pub options: ContextOptions,
    /// Global event cursor at capture time, not a live session attachment.
    pub through_sequence: u64,
    pub turns: Vec<ContextTurn>,
    pub omitted_turns: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SessionContextParams {
    pub session_id: String,
    /// Include completions at or before this global event sequence.
    #[serde(default)]
    pub through_sequence: Option<u64>,
    #[serde(default)]
    pub options: ContextOptions,
}

pub const MAX_SESSION_HISTORY_BYTES: u32 = 256 * 1024;
pub const MAX_SESSION_HISTORY_ENTRIES: u32 = 32;
pub const MAX_HISTORY_SEARCH_TURNS: u32 = 128;

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SessionHistoryGetParams {
    pub session_id: String,
    pub task_id: String,
    /// Optionally restrict the lookup to an earlier listing or search snapshot.
    #[serde(default)]
    pub through_sequence: Option<u64>,
    /// Compact JSON byte limit for the returned entry. Defaults to 64 KiB.
    #[serde(default)]
    pub max_bytes: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SessionHistoryListParams {
    pub session_id: String,
    /// Global event watermark. Required when continuing a page.
    #[serde(default)]
    pub through_sequence: Option<u64>,
    /// Exclusive completion cursor returned by the previous page.
    #[serde(default)]
    pub before_sequence: Option<u64>,
    /// Defaults to 16 and accepts 1 to 32 entries.
    #[serde(default)]
    pub limit: Option<u32>,
    /// Compact JSON byte limit for the entries array. Defaults to 64 KiB.
    #[serde(default)]
    pub max_bytes: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(
    tag = "kind",
    rename_all = "snake_case",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum SessionHistoryEntry {
    Turn {
        turn: ContextTurn,
    },
    /// The individual turn exceeded the requested budget even on an empty page.
    Oversized {
        task_id: String,
        completion_sequence: u64,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SessionHistoryListPage {
    pub through_sequence: u64,
    pub entries: Vec<SessionHistoryEntry>,
    pub next_before_sequence: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SessionHistoryRole {
    User,
    Assistant,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SessionHistorySearchParams {
    pub session_id: String,
    /// Literal substring in a user or assistant message, 1 to 256 UTF-8 bytes after trimming.
    pub query: String,
    /// Search only messages with this role. Omitting it searches both roles.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<SessionHistoryRole>,
    /// Reuse the first page's completion watermark when fetching more results.
    #[serde(default)]
    pub through_sequence: Option<u64>,
    /// Exclusive completion cursor returned by the previous page.
    #[serde(default)]
    pub before_sequence: Option<u64>,
    /// Match count per page. Defaults to 16 and accepts 1 to 32.
    #[serde(default)]
    pub limit: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkspaceHistorySearchParams {
    pub workspace_id: String,
    /// Literal substring in a user or assistant message, 1 to 256 UTF-8 bytes after trimming.
    pub query: String,
    #[serde(default)]
    pub role: Option<SessionHistoryRole>,
    #[serde(default)]
    pub through_sequence: Option<u64>,
    #[serde(default)]
    pub before_sequence: Option<u64>,
    #[serde(default)]
    pub limit: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SessionHistoryMatch {
    pub task_id: String,
    pub completion_sequence: u64,
    /// Included when searching across a workspace.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SessionHistorySearchPage {
    pub through_sequence: u64,
    pub matches: Vec<SessionHistoryMatch>,
    pub scanned_turns: u32,
    pub skipped_oversized_turns: u32,
    /// Continue even if a bounded scan produced no matches.
    pub next_before_sequence: Option<u64>,
}
