use crate::{ToolSpec, new_id};
use fliuno_protocol::{
    ContextMode, Message, ModelBudget, ModelBudgetInputBytes, ModelBudgetRejection,
    ModelCapabilitiesSource, ModelInputPreflight, ModelInputType, ModelUsage, Task, ToolCall,
    UserContentPart,
};
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
    pub preflight: Option<ModelInputPreflight>,
}

/// Whether a failed provider step could have reached the remote model service.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelError {
    /// Local validation or request construction failed before dispatch.
    NotDispatched(String),
    /// Input exceeded the configured budget before generation was dispatched.
    InputBudgetExceeded(InputBudgetError),
    /// Cancellation was observed before any request could be sent.
    CancelledBeforeDispatch,
    /// The request may have reached the provider, so retry needs explicit consent.
    OutcomeUnknown(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputBudgetError {
    pub message: String,
    pub report: Option<Box<ModelBudgetRejection>>,
}

impl InputBudgetError {
    pub(crate) fn terminal_message(&self) -> String {
        let mut message = format!(
            "model input exceeds its budget before generation: {}",
            self.message
        );
        if let Some(report) = &self.report {
            message.push_str(". See model_budget_rejected for measured size and limit.");
            if let Some(sizes) = &report.normalized_input_bytes {
                if sizes.current_media > 0 {
                    message.push_str(" Try smaller or fewer image/PDF attachments.");
                }
                if sizes.tool_definitions > 0 {
                    message.push_str(
                        " The host can reduce the number or schema size of registered tools.",
                    );
                }
                if sizes.history > 0 {
                    message.push_str(
                        " Start a new task with fewer history records or isolated context.",
                    );
                }
            }
        }
        message
    }
}

impl ModelError {
    /// Add bounded diagnostics outside the kernel lock, only on known rejection.
    pub fn with_input_attribution(mut self, task: &Task, tools: &[ToolSpec]) -> Self {
        if let Self::InputBudgetExceeded(error) = &mut self
            && let Some(report) = &mut error.report
        {
            report.normalized_input_bytes = input_attribution(task, tools).ok();
        }
        self
    }
}

pub(crate) fn input_attribution(
    task: &Task,
    tools: &[ToolSpec],
) -> crate::Result<ModelBudgetInputBytes> {
    fn size(value: &impl serde::Serialize) -> crate::Result<u64> {
        Ok(crate::json_size(value)? as u64)
    }

    let mut bytes = ModelBudgetInputBytes::default();
    if let Some(context) = &task.context
        && context.options.mode != ContextMode::Isolated
    {
        let turns = crate::context::tool_outputs::project(
            &context.turns,
            context.reduce_historical_tool_outputs,
        )?;
        for turn in turns.iter() {
            bytes.history += size(turn)?;
        }
        for summary in &context.summaries {
            bytes.history += size(&(&summary.text, &summary.sources))?;
        }
        if !task.context_memory_omitted_for_budget {
            for memory in &context.memories {
                bytes.memory += size(memory)?;
            }
        }
    }
    for message in &task.messages {
        match message {
            Message::User { text } => bytes.current_text += size(text)?,
            Message::UserParts { parts } => {
                for part in parts {
                    match part {
                        UserContentPart::Text { text } => bytes.current_text += size(text)?,
                        _ => bytes.current_media += size(part)?,
                    }
                }
            }
            Message::ToolResult { .. } => bytes.tool_results += size(message)?,
            Message::ProviderReplay { data, .. } => bytes.provider_replay += size(data)?,
            // The adapters already derive these from provider replay items.
            Message::Assistant { .. } | Message::ToolCall { .. } => {}
        }
    }
    for tool in tools {
        bytes.tool_definitions += size(&(&tool.name, &tool.description, &tool.input_schema))?;
    }
    Ok(bytes)
}

impl From<String> for InputBudgetError {
    fn from(message: String) -> Self {
        Self {
            message,
            report: None,
        }
    }
}

impl From<&str> for InputBudgetError {
    fn from(message: &str) -> Self {
        message.to_owned().into()
    }
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
            Self::InputBudgetExceeded(error) => formatter.write_str(&error.message),
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
            preflight: None,
        }
    }
}

/// Providers receive normalized messages; provider wire formats stay in adapters.
pub trait ModelProvider: Send + Sync {
    /// Stable, non-secret identity used to keep persisted run plans bound to
    /// the same model implementation after process restart.
    fn persistent_identity(&self) -> Option<(String, String)> {
        let (mut provider, model) = self.model_identity()?;
        if let Some(effort) = self.reasoning_effort() {
            provider.push_str("@effort=");
            provider.push_str(effort);
        }
        Some((provider, model))
    }

    fn reasoning_effort(&self) -> Option<&'static str> {
        None
    }

    fn configured_budget(&self) -> Option<ModelBudget> {
        None
    }

    fn supports_progress(&self) -> bool {
        false
    }

    fn supports_context_compaction(&self) -> bool {
        false
    }

    fn model_identity(&self) -> Option<(String, String)> {
        None
    }

    fn model_identity_source(&self) -> ModelCapabilitiesSource {
        ModelCapabilitiesSource::Configured
    }

    fn supported_input_types(&self) -> Vec<ModelInputType> {
        vec![ModelInputType::Text]
    }

    fn supported_input_types_source(&self) -> ModelCapabilitiesSource {
        ModelCapabilitiesSource::ProviderAdapter
    }

    /// Apply a persisted maximum undercount without reducing configured margins.
    fn set_calibrated_safety_margin(&self, _provider: &str, _model: &str, _tokens: u32) {}

    /// Observe a committed count-versus-usage sample.
    fn observe_preflight(&self, _preflight: &ModelInputPreflight, _actual_input_tokens: u64) {}

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
    fn persistent_identity(&self) -> Option<(String, String)> {
        Some(("demo".into(), "deterministic-v1".into()))
    }

    fn next(
        &self,
        task: &Task,
        _tools: &[ToolSpec],
        _cancellation: &CancellationToken,
    ) -> std::result::Result<ModelTurn, String> {
        if task
            .context
            .as_ref()
            .is_some_and(|context| context.options.plan_only)
        {
            return Ok(
                if task
                    .messages
                    .iter()
                    .any(|message| matches!(message, Message::ToolResult { .. }))
                {
                    ModelDecision::Complete("Demo plan ready for review.".into()).into()
                } else {
                    ModelDecision::Call(ToolCall {
                    id: new_id("call"),
                    name: crate::interaction::PLAN.into(),
                    arguments: json!({"steps":[
                        {"text":"Review the task and workspace context","status":"pending"},
                        {"text":"Confirm the proposed changes before implementation","status":"pending"}
                    ]}),
                }).into()
                },
            );
        }
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
