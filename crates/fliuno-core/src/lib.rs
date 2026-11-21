//! Persistent, UI-independent task runtime. The bundled provider is a demo.
mod agent;
mod agent_remote;
mod agent_tools;
mod approval;
mod authority;
pub use authority::authority_digest;
mod context;
pub mod diagnostics;
mod error;
mod events;
mod json_size;
pub use json_size::json_size;
mod policy;
mod provider;
mod query;
pub use query::MAX_TASK_LIST_TARGET_BYTES;
pub const MAX_APPROVAL_LIST_TARGET_BYTES: u32 = MAX_TASK_LIST_TARGET_BYTES;
mod registry;
mod runs;
mod storage;
mod supervision;
pub(crate) use storage::ToolInputMutation;
pub use supervision::{SupervisorLease, SupervisorTicket};
mod task;
mod tool;

pub use agent::{
    AgentBackend, AgentCommand, AgentObservation, AgentObserver, AgentOutcome, AgentRequest,
    PreparedAgentRun, PreparedAgentSessionList,
};
pub use agent_remote::{
    CompletedRemoteTaskCheck, IncomingAgentArtifact, IncomingAgentArtifactPart,
    PreparedRemoteTaskCheck,
};
pub use agent_tools::AgentToolObservation;
pub use approval::{ApprovalOutcome, PreparedToolExecution, ToolInputOutcome};
pub use context::{
    MAX_CONTEXT_BYTES, MAX_CONTEXT_CANDIDATES, MAX_CONTEXT_TURNS, context_prompt,
    task_context_prompt,
};
pub use error::{Error, Result};
pub use events::{
    EventReceiver, EventScope, EventWatermarkReceiver, PendingEventWatermark, ProgressPublisher,
    ProgressReceiver,
};
pub use policy::{AskForWrites, PolicyDecision, PolicyEngine};
pub use provider::{
    CancellationToken, DemoProvider, ModelDecision, ModelError, ModelProvider, ModelStreamEvent,
    ModelTurn, ProviderReplay,
};
pub use storage::MAX_EVENT_TARGET_BYTES;
pub use storage::TaskMemoryQuery;
pub use storage::{
    BackupEntry, BackupManifest, BackupProgress, BackupRetention, BackupRotationResult,
    BackupSource, McpSubscriptionAttempt, McpSubscriptionKey, McpTaskRecord, McpTaskStore,
};
pub use task::compaction::{
    execute as execute_context_compaction, pending as context_compaction_pending,
};
pub use task::{PreparedModelStep, compaction::ContextCompaction, start::TaskSummaryStart};
pub use tool::{Tool, ToolContext, ToolEffect, ToolError, ToolOutput, ToolSpec};

use fliuno_protocol::*;
use rusqlite::Connection;
use std::{
    collections::BTreeMap,
    fs::File,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};
use tool::CreateArtifact;
use uuid::Uuid;

pub type ModelProviderHandle = Arc<dyn ModelProvider>;

fn new_id(prefix: &str) -> String {
    format!("{prefix}_{}", Uuid::new_v4())
}

pub struct Kernel {
    db: Connection,
    provider: ModelProviderHandle,
    policy: Box<dyn PolicyEngine>,
    tools: BTreeMap<String, Arc<dyn Tool>>,
    agents: BTreeMap<String, Arc<dyn AgentBackend>>,
    agent_controls: BTreeMap<String, agent::AgentControl>,
    events: events::EventHub,
    progress: events::ProgressHub,
    // Keep the sidecar handle alive to prevent two daemons recovering the same DB.
    _lock: Option<Arc<File>>,
}

impl Kernel {
    pub fn model_provider_handle(&self) -> ModelProviderHandle {
        Arc::clone(&self.provider)
    }

    pub fn model_progress_supported(&self) -> bool {
        self.provider.supports_progress()
    }

    pub fn durable_runs_supported(&self) -> bool {
        self.provider.persistent_identity().is_some() && self.policy.persistent_identity().is_some()
    }

    pub fn configured_model_budget(&self) -> Option<ModelBudget> {
        self.provider.configured_budget()
    }

    pub fn configured_model_capabilities(&self) -> Option<ModelCapabilities> {
        let (provider, model) = self.provider.model_identity()?;
        Some(ModelCapabilities {
            provider,
            model,
            model_source: self.provider.model_identity_source(),
            supported_input_types: self.provider.supported_input_types(),
            input_types_source: self.provider.supported_input_types_source(),
            budget: self.provider.configured_budget(),
            automatic_context_compaction: self.provider.supports_context_compaction()
                && self.provider.configured_budget().is_some(),
            refreshed_at_ms: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis()
                .min(u64::MAX as u128) as u64,
        })
    }

    pub fn model_progress_publisher(&self) -> ProgressPublisher {
        ProgressPublisher::new(self.progress.clone())
    }

    pub fn subscribe_events(&self) -> Result<EventReceiver> {
        self.events.subscribe(events::EventScope::All)
    }

    /// Subscribe to committed events for one existing task.
    pub fn subscribe_task_events(&self, task_id: &str) -> Result<EventReceiver> {
        self.require_task(task_id)?;
        self.events
            .subscribe(events::EventScope::Task(task_id.into()))
    }

    /// Subscribe to committed events for one existing session.
    pub fn subscribe_session_events(&self, session_id: &str) -> Result<EventReceiver> {
        self.require_session(session_id)?;
        self.events
            .subscribe(events::EventScope::Session(session_id.into()))
    }

    pub fn subscribe_workspace_events(&self, workspace_id: &str) -> Result<EventReceiver> {
        self.require_workspace(workspace_id)?;
        self.events
            .subscribe(events::EventScope::Workspace(workspace_id.into()))
    }

    /// Subscribe to compact commit watermarks using the same event scope rules.
    pub fn subscribe_event_watermarks(&self, scope: EventScope) -> Result<EventWatermarkReceiver> {
        match &scope {
            EventScope::All => {}
            EventScope::Task(id) => self.require_task(id)?,
            EventScope::Session(id) => self.require_session(id)?,
            EventScope::Workspace(id) => self.require_workspace(id)?,
        }
        self.events.subscribe_watermarks(scope)
    }

    pub fn subscribe_model_progress(&self) -> Result<ProgressReceiver> {
        self.progress.subscribe(false)
    }

    /// Subscribe at a model step boundary so an in-flight suffix is never
    /// mistaken for a complete preview after a stream gap.
    pub fn subscribe_model_progress_from_next_step(&self) -> Result<ProgressReceiver> {
        self.progress.subscribe(true)
    }

    pub fn set_model_provider(&mut self, provider: Box<dyn ModelProvider>) {
        self.provider = Arc::from(provider);
    }

    pub fn set_policy(&mut self, policy: Box<dyn PolicyEngine>) {
        self.policy = policy;
    }
}

#[cfg(test)]
mod tests;
