//! Frozen, provider-neutral records of earlier completed work. These values
//! describe past effects; they never carry permissions or executable requests.
use crate::{AgentHostTool, ModelBudget, ModelInputPreflight, ModelUsage, TaskBackend, ToolCall};
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

pub const MAX_CONTEXT_SCAN_LIMIT: u32 = 512;
pub const MAX_EXPLICIT_CONTEXT_MEMORIES: usize = 8;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ContextOptions {
    #[serde(default)]
    pub mode: ContextMode,
    /// Disable workspace memory retrieval without discarding session history.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub disable_memory: bool,
    /// Expose local plan and clarification tools to this model task.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub enable_interaction: bool,
    /// Limit this model task to planning, clarification and trusted confined reads.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub plan_only: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions_revision: Option<u64>,
    /// Select exactly these revisions, without keyword or embedding retrieval.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(min = 1, max = MAX_EXPLICIT_CONTEXT_MEMORIES))]
    pub memory_selection: Option<Vec<ContextMemoryRef>>,
    /// Optional freshness weighting for automatically selected memories.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 1, max = 3650))]
    pub memory_recency_half_life_days: Option<u32>,
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
    /// Maximum completed records examined by fit_recent or match_recent.
    /// Omit to examine at most 128 records, including any skipped records.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 1, max = MAX_CONTEXT_SCAN_LIMIT))]
    pub scan_limit: Option<u32>,
    /// Include only records completed strictly after this event cursor.
    /// Omit or set to zero to consider the entire session history.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after_sequence: Option<u64>,
    /// Pin the inclusive completion watermark for preview and task creation.
    /// Omit to use the current watermark. Zero selects an empty history.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub through_sequence: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ContextMemoryRef {
    pub id: String,
    pub revision: u64,
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
            disable_memory: false,
            enable_interaction: false,
            plan_only: false,
            instructions_revision: None,
            memory_selection: None,
            memory_recency_half_life_days: None,
            max_turns: default_turns(),
            max_bytes: default_bytes(),
            selection: ContextSelection::Recent,
            detail: ContextDetail::Full,
            task_ids: None,
            history_query: None,
            scan_limit: None,
            after_sequence: None,
            through_sequence: None,
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

/// Workspace knowledge frozen into a task as untrusted reference data.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ContextMemory {
    pub id: String,
    pub key: String,
    pub content: String,
    pub revision: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_task_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub matched_by: Option<ContextMemoryMatch>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ContextMemoryMatch {
    Keyword,
    Semantic,
    Both,
    Explicit,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ContextSummarySource {
    pub task_id: String,
    pub completion_sequence: u64,
}

/// An untrusted projection of completed task records, not executable history.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ContextSummary {
    pub text: String,
    pub sources: Vec<ContextSummarySource>,
    pub prompt_version: u32,
    pub provider: String,
    pub model: String,
    pub budget: ModelBudget,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preflight: Option<ModelInputPreflight>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<ModelUsage>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TaskContext {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions: Option<crate::WorkspaceInstructions>,
    pub options: ContextOptions,
    /// Global event cursor at capture time, not a live session attachment.
    pub through_sequence: u64,
    pub turns: Vec<ContextTurn>,
    pub omitted_turns: u64,
    /// Bounded workspace memories selected at task creation.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub memories: Vec<ContextMemory>,
    /// Present when a requested semantic lookup fell back to keyword search.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_fallback: Option<crate::WorkspaceMemoryFallback>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub summaries: Vec<ContextSummary>,
    /// A confirmed input-budget rejection scheduled a separate model attempt.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub compaction_pending: bool,
    /// Project large tool outputs as excerpts after confirmed budget rejection.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub reduce_historical_tool_outputs: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SessionContextParams {
    pub session_id: String,
    /// Include completions at or before this global event sequence.
    /// If options.throughSequence is also given, the two values must agree.
    #[serde(default)]
    pub through_sequence: Option<u64>,
    #[serde(default)]
    pub options: ContextOptions,
}

pub const MAX_SESSION_HISTORY_BYTES: u32 = 256 * 1024;
pub const MAX_SESSION_HISTORY_ENTRIES: u32 = 32;
/// Default budget and the fixed cap on servers without scanLimit support.
pub const MAX_HISTORY_SEARCH_TURNS: u32 = 128;
pub const MAX_HISTORY_SEARCH_SCAN_LIMIT: u32 = 512;

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
    /// Exclusive lower bound, preserved on every continuation page.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after_sequence: Option<u64>,
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
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SessionHistorySubscribeParams {
    pub session_id: String,
    /// A previous fully consumed history watermark, exclusive.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after_sequence: Option<u64>,
    #[serde(default)]
    pub limit: Option<u32>,
    #[serde(default)]
    pub max_bytes: Option<u32>,
}

impl From<SessionHistorySubscribeParams> for SessionHistoryListParams {
    fn from(request: SessionHistorySubscribeParams) -> Self {
        Self {
            session_id: request.session_id,
            through_sequence: None,
            after_sequence: request.after_sequence,
            before_sequence: None,
            limit: request.limit,
            max_bytes: request.max_bytes,
        }
    }
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
    /// Exclusive lower bound. Use a previously consumed watermark to search new turns.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after_sequence: Option<u64>,
    /// Exclusive completion cursor returned by the previous page.
    #[serde(default)]
    pub before_sequence: Option<u64>,
    /// Match count per page. Defaults to 16 and accepts 1 to 32.
    #[serde(default)]
    pub limit: Option<u32>,
    /// Maximum completed turns examined per page. Defaults to 128.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 1, max = MAX_HISTORY_SEARCH_SCAN_LIMIT))]
    pub scan_limit: Option<u32>,
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
    /// Exclusive lower bound, preserved on every continuation page.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after_sequence: Option<u64>,
    #[serde(default)]
    pub before_sequence: Option<u64>,
    #[serde(default)]
    pub limit: Option<u32>,
    /// Maximum completed turns examined per page. Defaults to 128.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 1, max = MAX_HISTORY_SEARCH_SCAN_LIMIT))]
    pub scan_limit: Option<u32>,
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
