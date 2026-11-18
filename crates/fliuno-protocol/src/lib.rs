//! Fliuno application protocol and persisted domain values. No runtime dependencies.
mod context;
pub use context::*;
mod terminal;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
pub use terminal::*;

pub const PROTOCOL_VERSION: &str = "0.1";

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Workspace {
    pub id: String,
    pub root: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Session {
    pub id: String,
    pub workspace_id: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Running,
    WaitingApproval,
    Interrupted,
    Completed,
    Cancelled,
    Failed,
}

impl TaskStatus {
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Cancelled | Self::Failed)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Approval {
    pub id: String,
    pub task_id: String,
    pub call: ToolCall,
    /// Absent in legacy snapshots, which cannot authorize a new execution.
    #[serde(default)]
    pub authority: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalDecision {
    AllowOnce,
    Deny,
}

/// Host-registered execution target. Commands and credentials never enter RPC.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(
    tag = "type",
    rename_all = "snake_case",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum TaskBackend {
    #[default]
    Model,
    ExternalAgent {
        agent_id: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AgentSpec {
    pub id: String,
    pub protocol: String,
    pub isolation: String,
    #[serde(default)]
    pub workspace_access: bool,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AgentApprovalSource {
    #[default]
    AgentPermission,
    HostTool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AgentHostToolStatus {
    AwaitingApproval,
    Approved,
    Running,
    Succeeded,
    Denied,
    Failed,
    OutcomeUnknown,
    Closed,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AgentHostTool {
    pub id: String,
    pub call: ToolCall,
    pub status: AgentHostToolStatus,
    pub output: Option<Value>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AgentApproval {
    pub id: String,
    pub run_id: String,
    pub permission_id: String,
    #[serde(default)]
    pub source: AgentApprovalSource,
    /// Untrusted, read-only peer context. Never interpreted as host authority.
    pub request: Value,
    pub allow_once_available: bool,
    pub expires_at_unix_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AgentStopReason {
    EndTurn,
    Refusal,
    MaxTokens,
    MaxTurnRequests,
    Cancelled,
    Failed,
    Rejected,
    InputRequired,
    AuthRequired,
}

/// Neutral remote execution evidence, separate from the local task lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AgentTaskState {
    Submitted,
    Working,
    Completed,
    Failed,
    Cancelled,
    Rejected,
    InputRequired,
    AuthRequired,
}
impl AgentTaskState {
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Completed | Self::Failed | Self::Cancelled | Self::Rejected
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AgentRemoteTask {
    pub id: String,
    pub context_id: Option<String>,
    pub state: AgentTaskState,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AgentArtifactLink {
    pub source_id: String,
    pub artifact_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AgentRun {
    pub id: String,
    pub finished: bool,
    pub session_id: Option<String>,
    pub outcome_unknown: bool,
    pub stop_reason: Option<AgentStopReason>,
    pub text: String,
    pub update_count: u32,
    pub recorded_bytes: u64,
    pub permission_count: u32,
    pub pending_approvals: Vec<AgentApproval>,
    #[serde(default)]
    pub host_tools: Vec<AgentHostTool>,
    #[serde(default)]
    pub remote_task: Option<AgentRemoteTask>,
    #[serde(default)]
    pub artifacts: Vec<AgentArtifactLink>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(
    tag = "role",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum Message {
    User {
        text: String,
    },
    ToolCall {
        call: ToolCall,
    },
    ToolResult {
        call_id: String,
        output: Value,
    },
    Assistant {
        text: String,
    },
    /// Opaque output needed to replay provider reasoning/tool-use blocks exactly.
    ProviderReplay {
        provider: String,
        data: Value,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Task {
    pub id: String,
    pub session_id: String,
    pub status: TaskStatus,
    pub messages: Vec<Message>,
    /// None for pre-context snapshots; legacy retries retain their original input.
    #[serde(default)]
    pub context: Option<TaskContext>,
    pub pending_approval: Option<Approval>,
    #[serde(default)]
    pub in_flight_tool: Option<ToolCall>,
    /// A restart/timeout can leave the model request result and billing unknown.
    #[serde(default)]
    pub model_outcome_unknown: bool,
    pub output: Option<String>,
    pub model_steps: u32,
    #[serde(default)]
    pub backend: TaskBackend,
    /// Host registration captured at creation for a queued external-agent task.
    #[serde(default)]
    pub backend_authority: Option<String>,
    #[serde(default)]
    pub agent_run: Option<AgentRun>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Artifact {
    pub id: String,
    pub task_id: String,
    pub title: String,
    pub media_type: String,
    pub content: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(
    tag = "type",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum EventKind {
    TaskStarted,
    AgentStarted {
        run_id: String,
        agent_id: String,
    },
    AgentSessionOpened {
        run_id: String,
        session_id: String,
    },
    AgentUpdate {
        run_id: String,
        update: Value,
    },
    AgentRemoteTaskUpdated {
        run_id: String,
        task: AgentRemoteTask,
    },
    AgentApprovalRequested {
        approval: AgentApproval,
    },
    /// Durable intent; delivery to the peer is not guaranteed after a crash.
    AgentApprovalResolved {
        run_id: String,
        approval_id: String,
        decision: ApprovalDecision,
    },
    AgentApprovalClosed {
        run_id: String,
        approval_id: String,
    },
    AgentFinished {
        run_id: String,
        stop_reason: Option<AgentStopReason>,
        outcome_unknown: bool,
    },
    AgentHostToolRequested {
        run_id: String,
        operation: AgentHostTool,
    },
    AgentHostToolStarted {
        run_id: String,
        request_id: String,
    },
    AgentHostToolCompleted {
        run_id: String,
        request_id: String,
        status: AgentHostToolStatus,
        output: Option<Value>,
        error: Option<String>,
    },
    ModelStarted {
        step: u32,
    },
    ApprovalRequested {
        approval: Approval,
    },
    ApprovalResolved {
        approval_id: String,
        decision: ApprovalDecision,
    },
    ToolCompleted {
        call_id: String,
        output: Value,
    },
    ToolStarted {
        call: ToolCall,
    },
    ToolOutcomeUnknown {
        call_id: String,
        message: String,
        output: Option<Value>,
    },
    ArtifactCreated {
        artifact_id: String,
    },
    TaskCompleted {
        text: String,
    },
    TaskCancelled,
    TaskFailed {
        message: String,
    },
    TaskInterrupted,
    ModelOutcomeUnknown {
        step: u32,
    },
    TaskResumed,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Event {
    pub sequence: u64,
    pub task_id: String,
    pub data: EventKind,
}

/// Volatile model text preview. It is not stored in the durable event log.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(
    tag = "type",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum ModelProgress {
    TextDelta {
        task_id: String,
        step: u32,
        text: String,
    },
    Reset {
        task_id: String,
        step: u32,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct InitializeParams {
    pub protocol_version: String,
    pub client_name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkspaceOpenParams {
    pub root: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SessionCreateParams {
    pub workspace_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TaskStartParams {
    pub session_id: String,
    pub input: String,
    #[serde(default)]
    pub client_request_id: Option<String>,
    #[serde(default)]
    pub backend: TaskBackend,
    #[serde(default)]
    pub context: Option<ContextOptions>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ListParams {
    #[serde(default)]
    pub after: u64,
    #[serde(default = "default_limit")]
    pub limit: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SessionListParams {
    pub workspace_id: String,
    #[serde(default)]
    pub after: u64,
    #[serde(default = "default_limit")]
    pub limit: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TaskListParams {
    pub session_id: String,
    #[serde(default)]
    pub after: u64,
    #[serde(default = "default_limit")]
    pub limit: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ObjectPage<T> {
    pub items: Vec<T>,
    pub next_after: u64,
    pub has_more: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TaskIdParams {
    pub task_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TaskResumeParams {
    pub task_id: String,
    #[serde(default)]
    pub retry_uncertain_model_step: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ApprovalRespondParams {
    pub task_id: String,
    pub approval_id: String,
    pub decision: ApprovalDecision,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EventsListParams {
    pub task_id: String,
    #[serde(default)]
    pub after: u64,
    #[serde(default = "default_limit")]
    pub limit: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EventsListAllParams {
    #[serde(default)]
    pub after: u64,
    #[serde(default = "default_limit")]
    pub limit: u32,
}

fn default_limit() -> u32 {
    100
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EventsPage {
    pub events: Vec<Event>,
    pub next_after: u64,
    pub has_more: bool,
}

/// Named parameter and domain schemas, generated from the actual wire types.
pub fn schema_bundle() -> Value {
    serde_json::json!({
        "protocolVersion": PROTOCOL_VERSION,
        "methods": {
            "initialize": schemars::schema_for!(InitializeParams),
            "workspace/open": schemars::schema_for!(WorkspaceOpenParams),
            "workspace/list": schemars::schema_for!(ListParams),
            "session/create": schemars::schema_for!(SessionCreateParams),
            "session/list": schemars::schema_for!(SessionListParams),
            "session/context": schemars::schema_for!(SessionContextParams),
            "terminal/list": schemars::schema_for!(TerminalListParams),
            "terminal/get": schemars::schema_for!(TerminalGetParams),
            "terminal/output": schemars::schema_for!(TerminalOutputParams),
            "terminal/subscribe": schemars::schema_for!(TerminalSubscribeParams),
            "terminal/unsubscribe": schemars::schema_for!(TerminalUnsubscribeParams),
            "terminal/archive": schemars::schema_for!(TerminalArchiveParams),
            "terminal/archiveBatch": schemars::schema_for!(TerminalArchiveBatchParams),
            "terminal/archives": schemars::schema_for!(TerminalArchivesParams),
            "task/start": schemars::schema_for!(TaskStartParams),
            "task/list": schemars::schema_for!(TaskListParams),
            "task/get": schemars::schema_for!(TaskIdParams),
            "task/cancel": schemars::schema_for!(TaskIdParams),
            "task/resume": schemars::schema_for!(TaskResumeParams),
            "approval/respond": schemars::schema_for!(ApprovalRespondParams),
            "events/list": schemars::schema_for!(EventsListParams),
            "events/listAll": schemars::schema_for!(EventsListAllParams),
            "artifacts/list": schemars::schema_for!(TaskIdParams),
            "tools/list": {"type":"object","properties":{},"additionalProperties":false},
            "agents/list": {"type":"object","properties":{},"additionalProperties":false}
        },
        "types": {
            "Workspace": schemars::schema_for!(Workspace),
            "Session": schemars::schema_for!(Session),
            "Task": schemars::schema_for!(Task),
            "TaskContext": schemars::schema_for!(TaskContext),
            "AgentSpec": schemars::schema_for!(AgentSpec),
            "TerminalSnapshot": schemars::schema_for!(TerminalSnapshot),
            "TerminalOutput": schemars::schema_for!(TerminalOutput),
            "TerminalSubscription": schemars::schema_for!(TerminalSubscription),
            "TerminalOutputNotification": schemars::schema_for!(TerminalOutputNotification),
            "TerminalSubscriptionEnded": schemars::schema_for!(TerminalSubscriptionEnded),
            "TerminalArchive": schemars::schema_for!(TerminalArchive),
            "TerminalArchiveBatch": schemars::schema_for!(TerminalArchiveBatch),
            "TerminalArchivesPage": schemars::schema_for!(ObjectPage<TerminalArchive>),
            "Artifact": schemars::schema_for!(Artifact),
            "Event": schemars::schema_for!(Event),
            "ModelProgress": schemars::schema_for!(ModelProgress),
            "EventsPage": schemars::schema_for!(EventsPage)
        }
    })
}
