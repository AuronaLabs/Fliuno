//! Fliuno application protocol and persisted domain values. No runtime dependencies.
mod approval;
pub use approval::*;
mod artifacts;
pub use artifacts::*;
mod context;
pub use context::*;
mod schema;
pub use schema::schema_bundle;
mod session;
pub use session::*;
mod terminal;
mod workspace;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
pub use terminal::*;
pub use workspace::*;

pub const PROTOCOL_VERSION: &str = "0.1";
/// Includes the line delimiter on the application stdio transport.
pub const MAX_APP_REQUEST_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Workspace {
    pub id: String,
    pub root: String,
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
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::WaitingApproval => "waiting_approval",
            Self::Interrupted => "interrupted",
            Self::Completed => "completed",
            Self::Cancelled => "cancelled",
            Self::Failed => "failed",
        }
    }

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

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AgentIdParams {
    pub agent_id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AgentSessionListParams {
    pub agent_id: String,
    #[serde(default)]
    pub cursor: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AgentSessionListLookupParams {
    pub request_id: String,
}

#[derive(Debug, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AgentSessionListAccepted {
    pub request_id: String,
}

#[derive(Debug, Serialize, JsonSchema)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum AgentSessionListState {
    Pending,
    Complete { page: AgentSessionPage },
    Failed { reason: AgentSessionListFailure },
}

#[derive(Clone, Copy, Debug, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AgentSessionListFailure {
    Unsupported,
    Rejected,
    Unavailable,
    InvalidPage,
}

#[derive(Debug, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AgentSessionPage {
    pub sessions: Vec<AgentSessionDescriptor>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AgentSessionDescriptor {
    pub session_id: String,
    pub cwd: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub additional_directories: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
}

/// Locally available operations for one configured backend. A particular task
/// and peer still have to meet the operation's protocol preconditions.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AgentCapabilities {
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub agent_session_listing: bool,
    pub remote_task_checks: bool,
    pub remote_task_watch: bool,
    pub remote_task_continuation: bool,
    pub agent_session_continuation: bool,
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
    RemotePreflightFailed,
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
pub struct RemoteContinuation {
    pub source_task_id: String,
    pub expected: AgentRemoteTask,
    /// True once the adapter has permission to send, even if it has no reply.
    pub submission_started: bool,
}

/// An explicit new turn on an existing Agent session. Its ID comes from a
/// settled task under the same registration and local session.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AgentSessionContinuation {
    pub source_task_id: String,
    pub agent_session_id: String,
    /// True once the next prompt may have been dispatched.
    pub submission_started: bool,
}

/// A point-in-time check, separate from the original run's recorded outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum RemoteTaskCheckOutcome {
    Observed { state: AgentTaskState },
    Unavailable,
    IdentityMismatch,
    StateConflict,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RemoteTaskCheckAccepted {
    pub check_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RemoteTaskCheckLookupParams {
    pub task_id: String,
    pub check_id: String,
}

pub const MAX_REMOTE_TASK_WATCH_MS: u64 = 5_000;

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RemoteTaskCheckParams {
    pub task_id: String,
    /// Wait for a status change to a known remote task, up to this deadline.
    #[serde(default)]
    #[schemars(range(min = 1, max = MAX_REMOTE_TASK_WATCH_MS))]
    pub wait_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AgentArtifactLink {
    pub source_id: String,
    /// Zero-based position in the original remote container, if projected.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub part_index: Option<u32>,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_continuation: Option<RemoteContinuation>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_session_continuation: Option<AgentSessionContinuation>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TaskSummary {
    pub id: String,
    pub session_id: String,
    pub status: TaskStatus,
    pub backend: TaskBackend,
    /// At most 160 Unicode characters from the first user message.
    pub input_preview: String,
    pub pending_approval_count: u32,
    pub model_outcome_unknown: bool,
    pub agent_outcome_unknown: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Artifact {
    pub id: String,
    pub task_id: String,
    pub title: String,
    pub media_type: String,
    #[serde(flatten)]
    pub body: ArtifactBody,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ArtifactSummary {
    pub id: String,
    pub task_id: String,
    pub title: String,
    pub media_type: String,
    pub content_bytes: u64,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub binary: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ArtifactListParams {
    pub task_id: String,
    #[serde(default)]
    pub after: u64,
    #[serde(default = "default_limit")]
    pub limit: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ArtifactGetParams {
    pub task_id: String,
    pub artifact_id: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ModelInputCountMode {
    JsonBytesEstimate,
    Provider,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ModelBudgetSource {
    Configured,
    ProviderMetadata,
}

/// Host configuration for each model request, independent of task history caps.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ModelBudget {
    pub window_tokens: u32,
    pub max_input_tokens: u32,
    pub max_output_tokens: u32,
    pub count_mode: ModelInputCountMode,
    pub source: ModelBudgetSource,
}

/// Token counts reported by the provider for one model step.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ModelUsage {
    pub provider: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cached_input_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_output_tokens: Option<u64>,
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
    AgentRemoteContinuationSending {
        run_id: String,
    },
    AgentRemoteTaskChecked {
        run_id: String,
        check_id: String,
        outcome: RemoteTaskCheckOutcome,
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
    ModelUsageReported {
        step: u32,
        usage: ModelUsage,
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
pub struct WorkspaceIdParams {
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
pub struct TaskContinueRemoteParams {
    pub task_id: String,
    pub input: String,
    pub client_request_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TaskContinueAgentSessionParams {
    pub task_id: String,
    pub input: String,
    pub client_request_id: String,
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
    pub archived: Option<bool>,
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
    /// Compact items array byte target. A larger first task still advances the cursor.
    #[serde(default)]
    pub target_bytes: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TaskSummaryListParams {
    pub session_id: String,
    #[serde(default)]
    pub status: Option<TaskStatus>,
    #[serde(default)]
    pub after: u64,
    #[serde(default = "default_limit")]
    pub limit: u32,
    /// Compact items array byte target. A larger first summary still advances the cursor.
    #[serde(default)]
    pub target_bytes: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkspaceTaskSummaryListParams {
    pub workspace_id: String,
    /// The after cursor follows this ordering. Omit for ascending creation order.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub order: Option<WorkspaceTaskSummaryOrder>,
    #[serde(default)]
    pub status: Option<TaskStatus>,
    #[serde(default)]
    pub after: u64,
    #[serde(default = "default_limit")]
    pub limit: u32,
    /// Compact items array byte target. A larger first summary still advances the cursor.
    #[serde(default)]
    pub target_bytes: Option<u32>,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceTaskSummaryOrder {
    #[default]
    OldestFirst,
    NewestFirst,
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
pub struct EventGetParams {
    /// A positive global event sequence, as returned by replay or a live event.
    pub sequence: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EventsListParams {
    pub task_id: String,
    #[serde(default)]
    pub after: u64,
    #[serde(default)]
    pub through: Option<u64>,
    #[serde(default = "default_limit")]
    pub limit: u32,
    /// Target size of the compact events array. A single larger event still advances the cursor.
    #[serde(default)]
    pub target_bytes: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EventsListAllParams {
    #[serde(default)]
    pub after: u64,
    #[serde(default)]
    pub through: Option<u64>,
    #[serde(default = "default_limit")]
    pub limit: u32,
    #[serde(default)]
    pub target_bytes: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EventsListSessionParams {
    pub session_id: String,
    #[serde(default)]
    pub after: u64,
    #[serde(default)]
    pub through: Option<u64>,
    #[serde(default = "default_limit")]
    pub limit: u32,
    #[serde(default)]
    pub target_bytes: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EventsListWorkspaceParams {
    pub workspace_id: String,
    #[serde(default)]
    pub after: u64,
    #[serde(default)]
    pub through: Option<u64>,
    #[serde(default = "default_limit")]
    pub limit: u32,
    #[serde(default)]
    pub target_bytes: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EventsSetScopeParams {
    #[serde(default)]
    pub task_id: Option<String>,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub workspace_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EventSubscription {
    /// Replay through this sequence. Live notifications cover later events.
    pub through_sequence: u64,
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
