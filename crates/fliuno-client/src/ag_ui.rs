//! Bind AG-UI requests to tasks and project committed events.
mod run;
mod sse;
mod stream;

pub use run::{AgUiRun, AgUiRunError};
pub use sse::{AG_UI_SSE_CONTENT_TYPE, AgUiSseError, AgUiSseStream};
pub use stream::{AgUiSseFeed, AgUiSseFeedError};

use fliuno_protocol::{Event, EventKind, TaskSummary};
use serde::Serialize;
use serde_json::{Value, json};

#[derive(Debug, Serialize)]
#[serde(
    tag = "type",
    rename_all = "SCREAMING_SNAKE_CASE",
    rename_all_fields = "camelCase"
)]
pub enum AgUiEvent {
    RunStarted {
        thread_id: String,
        run_id: String,
    },
    RunFinished {
        thread_id: String,
        run_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        outcome: Option<AgUiRunOutcome>,
    },
    RunError {
        message: String,
        code: &'static str,
    },
    TextMessageStart {
        message_id: String,
        role: &'static str,
    },
    TextMessageContent {
        message_id: String,
        delta: String,
    },
    TextMessageEnd {
        message_id: String,
    },
    Custom {
        name: &'static str,
        value: Value,
    },
}

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgUiRunOutcome {
    Cancelled,
}

/// The phase of the latest execution segment in this task's projected history.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgUiRunPhase {
    AwaitingStart,
    Running,
    Interrupted,
    Finished,
}

#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum AgUiProjectionError {
    #[error("event belongs to another task")]
    WrongTask,
    #[error("event sequence is not increasing")]
    Sequence,
    #[error("event does not match the projected run lifecycle")]
    Lifecycle,
}

#[derive(Clone, Copy)]
enum RunState {
    Pending,
    Running { start_sequence: u64, initial: bool },
    Interrupted,
    Finished,
}

/// Consume one task's committed events in order, starting with `TaskStarted`.
/// IDs are derived from the persisted task identity and event sequence.
/// Keep this projector as replay hands off to live events for the same task.
pub struct AgUiProjector {
    task_id: String,
    thread_id: String,
    external_run_id: Option<String>,
    sequence: u64,
    state: RunState,
}

impl AgUiProjector {
    pub fn new(task: &TaskSummary) -> Self {
        Self {
            task_id: task.id.clone(),
            thread_id: task.session_id.clone(),
            external_run_id: None,
            sequence: 0,
            state: RunState::Pending,
        }
    }

    fn for_external_run(task: &TaskSummary, run_id: &str) -> Self {
        Self {
            external_run_id: Some(run_id.to_owned()),
            ..Self::new(task)
        }
    }

    pub fn run_phase(&self) -> AgUiRunPhase {
        match self.state {
            RunState::Pending => AgUiRunPhase::AwaitingStart,
            RunState::Running { .. } => AgUiRunPhase::Running,
            RunState::Interrupted => AgUiRunPhase::Interrupted,
            RunState::Finished => AgUiRunPhase::Finished,
        }
    }

    /// One source event yields zero to four output events. A rejected input
    /// leaves the projector at the last accepted event.
    pub fn project(&mut self, event: Event) -> Result<Vec<AgUiEvent>, AgUiProjectionError> {
        if event.task_id != self.task_id {
            return Err(AgUiProjectionError::WrongTask);
        }
        if event.sequence <= self.sequence {
            return Err(AgUiProjectionError::Sequence);
        }
        let sequence = event.sequence;
        let output = match (event.data, self.state) {
            (EventKind::TaskStarted, RunState::Pending)
            | (EventKind::TaskResumed, RunState::Interrupted) => {
                let initial = matches!(self.state, RunState::Pending);
                self.state = RunState::Running {
                    start_sequence: sequence,
                    initial,
                };
                vec![AgUiEvent::RunStarted {
                    thread_id: self.thread_id.clone(),
                    run_id: self.run_id(sequence, initial),
                }]
            }
            (
                EventKind::TaskCompleted { text },
                RunState::Running {
                    start_sequence,
                    initial,
                },
            ) => {
                self.state = RunState::Finished;
                let mut output = Vec::with_capacity(if text.is_empty() { 1 } else { 4 });
                if !text.is_empty() {
                    let message_id = format!("{}:message:{sequence}", self.task_id);
                    output.extend([
                        AgUiEvent::TextMessageStart {
                            message_id: message_id.clone(),
                            role: "assistant",
                        },
                        AgUiEvent::TextMessageContent {
                            message_id: message_id.clone(),
                            delta: text,
                        },
                        AgUiEvent::TextMessageEnd { message_id },
                    ]);
                }
                output.push(AgUiEvent::RunFinished {
                    thread_id: self.thread_id.clone(),
                    run_id: self.run_id(start_sequence, initial),
                    outcome: None,
                });
                output
            }
            (EventKind::TaskFailed { message }, RunState::Running { .. }) => {
                self.state = RunState::Finished;
                vec![AgUiEvent::RunError {
                    message,
                    code: "fliuno.task_failed",
                }]
            }
            (EventKind::TaskInterrupted, RunState::Running { .. }) => {
                self.state = RunState::Interrupted;
                vec![AgUiEvent::RunError {
                    message: "Task interrupted".into(),
                    code: "fliuno.task_interrupted",
                }]
            }
            (
                EventKind::TaskCancelled,
                RunState::Running {
                    start_sequence,
                    initial,
                },
            ) => {
                self.state = RunState::Finished;
                vec![AgUiEvent::RunFinished {
                    thread_id: self.thread_id.clone(),
                    run_id: self.run_id(start_sequence, initial),
                    outcome: Some(AgUiRunOutcome::Cancelled),
                }]
            }
            (EventKind::TaskCancelled, RunState::Interrupted) => {
                self.state = RunState::Finished;
                vec![custom(
                    "fliuno.task_cancelled",
                    json!({"taskId": self.task_id}),
                )]
            }
            (
                EventKind::TaskStarted
                | EventKind::TaskResumed
                | EventKind::TaskCompleted { .. }
                | EventKind::TaskFailed { .. }
                | EventKind::TaskInterrupted
                | EventKind::TaskCancelled,
                _,
            )
            | (_, RunState::Pending) => return Err(AgUiProjectionError::Lifecycle),
            (EventKind::ApprovalRequested { approval }, _) => vec![custom(
                "fliuno.approval_requested",
                json!({"taskId": self.task_id, "approvalId": approval.id, "toolName": approval.call.name}),
            )],
            (EventKind::AgentApprovalRequested { approval }, _) => vec![custom(
                "fliuno.approval_requested",
                json!({"taskId": self.task_id, "approvalId": approval.id, "agentRunId": approval.run_id}),
            )],
            (
                EventKind::ApprovalResolved {
                    approval_id,
                    decision,
                }
                | EventKind::AgentApprovalResolved {
                    approval_id,
                    decision,
                    ..
                },
                _,
            ) => vec![custom(
                "fliuno.approval_resolved",
                json!({"taskId": self.task_id, "approvalId": approval_id, "decision": decision}),
            )],
            (EventKind::AgentApprovalClosed { approval_id, .. }, _) => vec![custom(
                "fliuno.approval_closed",
                json!({"taskId": self.task_id, "approvalId": approval_id}),
            )],
            (EventKind::ArtifactCreated { artifact_id }, _) => vec![custom(
                "fliuno.artifact_created",
                json!({"taskId": self.task_id, "artifactId": artifact_id}),
            )],
            (EventKind::ModelUsageReported { step, usage }, _) => vec![custom(
                "fliuno.model_usage_reported",
                json!({"taskId": self.task_id, "step": step, "usage": usage}),
            )],
            _ => Vec::new(),
        };
        self.sequence = sequence;
        Ok(output)
    }

    fn run_id(&self, start_sequence: u64, initial: bool) -> String {
        if initial && let Some(run_id) = &self.external_run_id {
            return run_id.clone();
        }
        format!("{}:run:{start_sequence}", self.task_id)
    }
}

fn custom(name: &'static str, value: Value) -> AgUiEvent {
    AgUiEvent::Custom { name, value }
}

#[cfg(test)]
mod tests;
