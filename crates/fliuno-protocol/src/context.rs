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
    #[serde(default)]
    pub options: ContextOptions,
}
