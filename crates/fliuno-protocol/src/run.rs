//! Durable, session-scoped task graphs.
use crate::{ContextOptions, MAX_TASK_MODEL_STEPS};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

pub const MAX_RUN_NODES: usize = 32;
pub const MAX_RUN_NODE_ID_BYTES: usize = 64;
pub const MAX_RUN_CONCURRENCY: u32 = 8;
pub const MAX_RUN_EVENT_PAGE: u32 = 256;
pub const MAX_RUN_TIMEOUT_MS: u32 = 86_400_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Running,
    Completed,
    PartiallyCompleted,
    Failed,
    Cancelled,
    TimedOut,
    Interrupted,
}

impl RunStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Completed => "completed",
            Self::PartiallyCompleted => "partially_completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::TimedOut => "timed_out",
            Self::Interrupted => "interrupted",
        }
    }

    pub const fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Completed
                | Self::PartiallyCompleted
                | Self::Failed
                | Self::Cancelled
                | Self::TimedOut
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RunNodeStatus {
    Waiting,
    Running,
    WaitingApproval,
    WaitingInput,
    Interrupted,
    Completed,
    Failed,
    Cancelled,
    TimedOut,
    Skipped,
}

impl RunNodeStatus {
    pub const fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Completed | Self::Failed | Self::Cancelled | Self::TimedOut | Self::Skipped
        )
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RunNodeInput {
    #[schemars(length(min = 1, max = MAX_RUN_NODE_ID_BYTES), regex(pattern = "^[A-Za-z0-9_-]+$"))]
    pub node_id: String,
    #[schemars(length(min = 1, max = crate::MAX_TASK_TEXT_BYTES))]
    pub input: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(length(max = MAX_RUN_NODES))]
    pub depends_on: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RunStartParams {
    pub session_id: String,
    #[schemars(length(min = 1, max = crate::MAX_CLIENT_REQUEST_ID_BYTES), regex(pattern = "^[!-~]+$"))]
    pub client_request_id: String,
    #[serde(default = "default_run_concurrency")]
    #[schemars(range(min = 1, max = MAX_RUN_CONCURRENCY))]
    pub max_concurrency: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 1, max = MAX_TASK_MODEL_STEPS))]
    pub max_model_steps: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 1, max = MAX_RUN_TIMEOUT_MS))]
    pub timeout_ms: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<ContextOptions>,
    #[schemars(length(min = 1, max = MAX_RUN_NODES))]
    pub nodes: Vec<RunNodeInput>,
}

fn default_run_concurrency() -> u32 {
    2
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RunNode {
    pub node_id: String,
    pub depends_on: Vec<String>,
    pub status: RunNodeStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Run {
    pub id: String,
    pub session_id: String,
    pub status: RunStatus,
    pub max_concurrency: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deadline_at_unix_ms: Option<u64>,
    pub revision: u64,
    pub nodes: Vec<RunNode>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RunIdParams {
    pub run_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RunCancelParams {
    pub run_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RunEventsParams {
    pub run_id: String,
    #[serde(default)]
    pub after: u64,
    #[serde(default = "default_run_event_limit")]
    #[schemars(range(min = 1, max = MAX_RUN_EVENT_PAGE))]
    pub limit: u32,
}

fn default_run_event_limit() -> u32 {
    64
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RunEvent {
    pub sequence: u64,
    pub run_id: String,
    pub data: RunEventKind,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(
    tag = "type",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum RunEventKind {
    Created,
    NodeStarted {
        node_id: String,
        task_id: String,
    },
    NodeStatusChanged {
        node_id: String,
        status: RunNodeStatus,
    },
    NodeSkipped {
        node_id: String,
        failed_dependencies: Vec<String>,
    },
    Completed {
        status: RunStatus,
    },
    Cancelled,
    TimedOut {
        deadline_at_unix_ms: u64,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RunEventsPage {
    pub events: Vec<RunEvent>,
    pub next_after: u64,
    pub through_sequence: u64,
}
