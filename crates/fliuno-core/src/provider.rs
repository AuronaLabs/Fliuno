use crate::{ToolSpec, new_id};
use fliuno_protocol::{Message, ModelBudget, ModelUsage, Task, ToolCall};
use serde_json::json;

pub use tokio_util::sync::CancellationToken;

pub enum ModelDecision {
    Call(ToolCall),
    Complete(String),
}

pub struct ProviderReplay {
    pub provider: String,
    pub data: serde_json::Value,
}

/// Continuation data is persisted with the decision, including reasoning blocks.
pub struct ModelTurn {
    pub decision: ModelDecision,
    pub replay: Option<ProviderReplay>,
    pub usage: Option<ModelUsage>,
}

/// Whether a failed provider step could have reached the remote model service.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelError {
    /// Local validation or request construction failed before dispatch.
    NotDispatched(String),
    /// Cancellation was observed before any request could be sent.
    CancelledBeforeDispatch,
    /// The request may have reached the provider, so retry needs explicit consent.
    OutcomeUnknown(String),
}

impl From<String> for ModelError {
    fn from(message: String) -> Self {
        Self::OutcomeUnknown(message)
    }
}

impl From<&str> for ModelError {
    fn from(message: &str) -> Self {
        Self::OutcomeUnknown(message.into())
    }
}

impl std::fmt::Display for ModelError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotDispatched(message) | Self::OutcomeUnknown(message) => {
                formatter.write_str(message)
            }
            Self::CancelledBeforeDispatch => {
                formatter.write_str("provider request cancelled before dispatch")
            }
        }
    }
}

impl std::error::Error for ModelError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelStreamEvent {
    TextDelta(String),
    Reset,
}

impl From<ModelDecision> for ModelTurn {
    fn from(decision: ModelDecision) -> Self {
        Self {
            decision,
            replay: None,
            usage: None,
        }
    }
}

/// Providers receive normalized messages; provider wire formats stay in adapters.
pub trait ModelProvider: Send + Sync {
    fn configured_budget(&self) -> Option<ModelBudget> {
        None
    }

    fn supports_progress(&self) -> bool {
        false
    }

    fn next(
        &self,
        task: &Task,
        tools: &[ToolSpec],
        cancellation: &CancellationToken,
    ) -> std::result::Result<ModelTurn, String>;

    /// Classify a provider failure when the adapter can establish whether the
    /// request was sent. Legacy providers default to the conservative unknown.
    fn next_classified(
        &self,
        task: &Task,
        tools: &[ToolSpec],
        cancellation: &CancellationToken,
    ) -> std::result::Result<ModelTurn, ModelError> {
        self.next(task, tools, cancellation)
            .map_err(ModelError::OutcomeUnknown)
    }

    /// Stream display-only updates when supported; canonical task state still
    /// changes only after the complete model turn is validated.
    fn next_with_progress(
        &self,
        task: &Task,
        tools: &[ToolSpec],
        cancellation: &CancellationToken,
        _progress: &mut dyn FnMut(ModelStreamEvent),
    ) -> std::result::Result<ModelTurn, String> {
        self.next(task, tools, cancellation)
    }

    /// Streaming counterpart to `next_classified`.
    fn next_with_progress_classified(
        &self,
        task: &Task,
        tools: &[ToolSpec],
        cancellation: &CancellationToken,
        progress: &mut dyn FnMut(ModelStreamEvent),
    ) -> std::result::Result<ModelTurn, ModelError> {
        self.next_with_progress(task, tools, cancellation, progress)
            .map_err(ModelError::OutcomeUnknown)
    }
}

/// Deterministic fixture: requests one text artifact, then reports its creation.
/// It does not call a model or interpret the user's request intelligently.
pub struct DemoProvider;

impl ModelProvider for DemoProvider {
    fn next(
        &self,
        task: &Task,
        _tools: &[ToolSpec],
        _cancellation: &CancellationToken,
    ) -> std::result::Result<ModelTurn, String> {
        if task
            .messages
            .iter()
            .any(|m| matches!(m, Message::ToolResult { .. }))
        {
            return Ok(ModelDecision::Complete("Demo artifact created.".into()).into());
        }
        let input = task
            .messages
            .iter()
            .find_map(|m| match m {
                Message::User { text } => Some(std::borrow::Cow::Borrowed(text.as_str())),
                Message::UserParts { parts } => Some(std::borrow::Cow::Owned(
                    parts
                        .iter()
                        .map(fliuno_protocol::UserContentPart::text_projection)
                        .collect(),
                )),
                _ => None,
            })
            .ok_or("task has no user message")?;
        Ok(ModelDecision::Call(ToolCall {
            id: new_id("call"),
            name: "artifact_create".into(),
            arguments: json!({"title": "Demo artifact", "content": input}),
        })
        .into())
    }
}
