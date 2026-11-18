//! Bounded native lifecycle evidence. Never serialize `io::Error` display text:
//! OS/library messages may embed host paths, arguments or environment values.
use serde::Serialize;
use std::io;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProcessOperation {
    StdinWrite,
    StdinShutdown,
    StdoutRead,
    StderrRead,
    PollExit,
    Terminate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProcessStopCause {
    RootExited,
    Deadline,
    Cancelled,
    IoError,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProcessFault {
    pub operation: ProcessOperation,
    pub kind: String,
    pub os_code: Option<i32>,
}
impl ProcessFault {
    pub(super) fn new(operation: ProcessOperation, error: &io::Error) -> Self {
        Self {
            operation,
            kind: format!("{:?}", error.kind()),
            os_code: error.raw_os_error(),
        }
    }
}

#[derive(Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProcessEvidence {
    /// Part of the total command budget, not a fresh budget after launch.
    pub launch_ms: u64,
    /// First reason for stopping; a later drain timeout never erases it.
    pub stop_cause: Option<ProcessStopCause>,
    pub stop_request_succeeded: Option<bool>,
    pub root_reaped: bool,
    pub stdin_closed: bool,
    /// Native startup returned after cancellation/deadline, before any input IO.
    pub stdin_skipped: bool,
    /// A previously polled writer was dropped on stop; background pipe IO may
    /// still finish, so this alone never confirms that the native handle closed.
    pub stdin_aborted: bool,
    pub stdout_closed: bool,
    pub stderr_closed: bool,
    pub cleanup_expired: bool,
    /// At most one fixed-size diagnostic per operation (six operations).
    pub faults: Vec<ProcessFault>,
}
impl ProcessEvidence {
    pub(super) fn record(&mut self, fault: ProcessFault) {
        if !self.faults.iter().any(|f| f.operation == fault.operation) {
            self.faults.push(fault);
        }
    }
}
