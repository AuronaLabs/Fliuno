use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AgentTerminalLoginStart {
    pub task_id: String,
    pub agent_id: String,
    pub rows: u16,
    pub cols: u16,
}

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AgentTerminalLoginHandle {
    pub login_id: String,
}

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AgentTerminalLoginOutputParams {
    pub login_id: String,
    pub after: u64,
}

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AgentTerminalLoginInput {
    pub login_id: String,
    /// Starts at zero. The server accepts each sequence at most once.
    pub sequence: u64,
    pub data: String,
}

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AgentTerminalLoginInputAccepted {
    pub next_sequence: u64,
}

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(
    tag = "state",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum AgentTerminalLoginStatus {
    Starting,
    Running,
    Closing,
    Closed,
    Exited { exit_code: i64 },
    Expired,
    Failed,
    OutcomeUnknown,
}

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AgentTerminalLoginOutput {
    pub status: AgentTerminalLoginStatus,
    pub next: u64,
    pub truncated: bool,
    pub data_base64: String,
}
