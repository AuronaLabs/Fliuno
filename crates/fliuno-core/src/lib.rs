//! Persistent, UI-independent task runtime. The bundled provider is a demo.
mod agent;
mod agent_remote;
mod agent_tools;
mod approval;
mod authority;
pub use authority::authority_digest;
mod context;
mod error;
mod events;
mod policy;
mod provider;
mod query;
mod registry;
mod storage;
mod task;
mod tool;

pub use agent::{
    AgentBackend, AgentCommand, AgentObservation, AgentObserver, AgentOutcome, AgentRequest,
    PreparedAgentRun,
};
pub use agent_remote::IncomingAgentArtifact;
pub use agent_tools::AgentToolObservation;
pub use approval::{ApprovalOutcome, PreparedToolExecution};
pub use context::{MAX_CONTEXT_BYTES, MAX_CONTEXT_TURNS, context_prompt};
pub use error::{Error, Result};
pub use events::{EventReceiver, ProgressPublisher, ProgressReceiver};
pub use policy::{AskForWrites, PolicyDecision, PolicyEngine};
pub use provider::{
    CancellationToken, DemoProvider, ModelDecision, ModelError, ModelProvider, ModelStreamEvent,
    ModelTurn, ProviderReplay,
};
pub use task::PreparedModelStep;
pub use tool::{Tool, ToolContext, ToolEffect, ToolError, ToolOutput, ToolSpec};

use fliuno_protocol::*;
use rusqlite::Connection;
use std::{collections::BTreeMap, fs::File, sync::Arc};
use tool::CreateArtifact;
use uuid::Uuid;

const MAX_MODEL_STEPS: u32 = 8;
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
    _lock: Option<File>,
}

impl Kernel {
    pub fn model_provider_handle(&self) -> ModelProviderHandle {
        Arc::clone(&self.provider)
    }

    pub fn model_progress_supported(&self) -> bool {
        self.provider.supports_progress()
    }

    pub fn model_progress_publisher(&self) -> ProgressPublisher {
        ProgressPublisher::new(self.progress.clone())
    }

    pub fn subscribe_events(&self) -> Result<EventReceiver> {
        self.events.subscribe()
    }

    pub fn subscribe_model_progress(&self) -> Result<ProgressReceiver> {
        self.progress.subscribe()
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
