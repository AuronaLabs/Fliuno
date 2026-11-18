//! Private, versioned local IPC. No executable or environment can be replaced
//! after startup. A fresh capability token authenticates every request.
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

pub const FRAME_LIMIT: usize = 128 * 1024;
pub const READ_LIMIT: usize = 32 * 1024;
pub const INPUT_LIMIT: usize = 16 * 1024;

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Descriptor {
    pub version: u32,
    pub endpoint: String,
    pub token: String,
    /// Diagnostic only. Never used as authority for reattachment or signaling.
    #[serde(default)]
    pub supervisor_pid: u32,
}

pub use fliuno_protocol::{
    TerminalOutput as Output, TerminalSnapshot as Snapshot, TerminalStatus as Status,
};

#[derive(Clone, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "snake_case",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum Operation {
    Status,
    Output {
        after: u64,
        limit: usize,
    },
    Subscribe {
        after: u64,
        limit: usize,
    },
    Write {
        operation_id: String,
        data: String,
    },
    Resize {
        operation_id: String,
        rows: u16,
        cols: u16,
    },
    Kill {
        operation_id: String,
    },
    Release {
        operation_id: String,
    },
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Request {
    pub version: u32,
    pub token: String,
    pub session_id: String,
    pub operation: Operation,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Response {
    pub result: Option<serde_json::Value>,
    pub error: Option<String>,
}

/// Bootstrap data travels via inherited stdin, never argv or a public RPC.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Launch {
    pub directory: PathBuf,
    pub descriptor: Descriptor,
    pub command: PathBuf,
    pub args: Vec<String>,
    pub environment: std::collections::BTreeMap<String, String>,
    pub executable_digest: String,
    pub resources: fliuno_sandbox::ResourceLimits,
    pub linux_cgroup_parent: Option<PathBuf>,
    pub cwd: PathBuf,
    pub spool_bytes: usize,
    pub lifetime_ms: u64,
    pub snapshot: Snapshot,
}

pub fn dimensions(rows: u16, cols: u16) -> Result<(), String> {
    if !(1..=500).contains(&rows) || !(1..=500).contains(&cols) {
        return Err("rows and cols must be 1..500".into());
    }
    Ok(())
}

pub(crate) fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u64::MAX as u128) as u64
}
