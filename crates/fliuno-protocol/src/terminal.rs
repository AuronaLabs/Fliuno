//! Transport-neutral terminal identity and byte cursors. Native PTY handles,
//! IPC credentials, executable paths and process PIDs are never wire authority.
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TerminalStatus {
    Starting,
    Running,
    /// Startup was refused before a PTY child could be launched.
    Failed,
    Exited,
    Killed,
    Expired,
    OutcomeUnknown,
    Released,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TerminalSnapshot {
    pub terminal_id: String,
    pub session_id: String,
    pub preset_id: String,
    pub status: TerminalStatus,
    /// Bounded, host-authored startup diagnostic; never child stderr or argv.
    /// Older records without this field remain readable.
    #[serde(default)]
    pub startup_error: Option<String>,
    /// True only when this response was received from the live supervisor.
    /// Offline reads never infer liveness from a saved PID or lock filename.
    pub supervisor_connected: bool,
    pub rows: u16,
    pub cols: u16,
    pub exit_code: Option<u32>,
    pub signal: Option<String>,
    pub start_offset: u64,
    pub end_offset: u64,
    pub input_bytes: u64,
    /// Root exit does not imply that every byte of PTY output was drained.
    pub output_closed: bool,
    pub expires_at_unix_ms: u64,
    /// A durable tombstone replaces this terminal's output and operation log.
    /// The original ID remains permanently reserved. Older spools omit it.
    #[serde(default)]
    pub archived_at_unix_ms: Option<u64>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TerminalOutput {
    pub snapshot: TerminalSnapshot,
    pub from: u64,
    pub next: u64,
    pub gap: bool,
    /// PTY bytes include escape sequences and may cut across UTF-8 boundaries.
    pub data_base64: String,
}

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TerminalListParams {
    pub session_id: String,
}

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TerminalGetParams {
    pub session_id: String,
    pub terminal_id: String,
}

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TerminalOutputParams {
    pub session_id: String,
    pub terminal_id: String,
    #[serde(default)]
    pub after: u64,
    #[serde(default = "limit")]
    pub limit: usize,
}
#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TerminalTailParams {
    pub session_id: String,
    pub terminal_id: String,
    #[serde(default = "limit")]
    pub limit: usize,
}

/// Maximum raw bytes in one terminal output page, before base64 encoding.
pub const MAX_TERMINAL_OUTPUT_BYTES: usize = 32 * 1024;

fn limit() -> usize {
    MAX_TERMINAL_OUTPUT_BYTES
}

/// Subscription IDs belong to one app-server connection and are never authority
/// to control a terminal. Reconnect with a new subscription and a byte cursor.
pub type TerminalSubscribeParams = TerminalOutputParams;

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TerminalUnsubscribeParams {
    pub subscription_id: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TerminalSubscription {
    pub subscription_id: String,
    pub session_id: String,
    pub terminal_id: String,
    pub after: u64,
}

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TerminalOutputNotification {
    pub subscription_id: String,
    pub output: TerminalOutput,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TerminalSubscriptionEndReason {
    Closed,
    SupervisorUnavailable,
    Overflow,
    Failed,
    Archived,
}

pub type TerminalArchiveParams = TerminalGetParams;

pub const MAX_TERMINAL_ARCHIVE_BATCH: usize = 32;

/// Explicit host-client selection. Validate the complete list before effects;
/// a batch does not infer retention policy or terminate live supervisors.
#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TerminalArchiveBatchParams {
    pub session_id: String,
    #[schemars(length(min = 1, max = 32))]
    pub terminal_ids: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TerminalArchiveBatch {
    /// Results follow request order. Each archive commits independently.
    pub items: Vec<TerminalArchiveBatchItem>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(
    tag = "status",
    rename_all = "snake_case",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum TerminalArchiveBatchItem {
    Archived {
        terminal_id: String,
        archive: Box<TerminalArchive>,
    },
    Failed {
        terminal_id: String,
        message: String,
    },
    /// Inspect this ID through terminal/get or terminal/archives. A lost commit
    /// response must not be reported as a known failure or an empty archive.
    OutcomeUnknown {
        terminal_id: String,
        message: String,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TerminalArchive {
    pub snapshot: TerminalSnapshot,
    /// The tombstone is committed even when a file is temporarily still open.
    /// Retry archive to complete bounded cleanup; never recreate the terminal.
    pub cleanup_pending: bool,
}

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TerminalArchivesParams {
    pub session_id: String,
    #[serde(default)]
    pub after: u64,
    #[serde(default = "archive_limit")]
    pub limit: u32,
}
fn archive_limit() -> u32 {
    100
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TerminalSubscriptionEnded {
    pub subscription_id: String,
    pub session_id: String,
    pub terminal_id: String,
    /// Last byte cursor emitted on this connection, including retention gaps.
    pub after: u64,
    pub reason: TerminalSubscriptionEndReason,
}
