use crate::ToolSpec;
use fliuno_protocol::ToolCall;

pub enum PolicyDecision {
    Ask,
    Deny(String),
}

/// Evaluates host-defined metadata, never model-supplied effect claims.
pub trait PolicyEngine: Send {
    fn evaluate(&self, spec: &ToolSpec, call: &ToolCall) -> PolicyDecision;

    /// Stable, non-secret identity for persistent work. Custom policies that
    /// omit it remain usable for ordinary tasks but cannot start durable runs.
    fn persistent_identity(&self) -> Option<String> {
        None
    }
}

pub struct AskForWrites;

impl PolicyEngine for AskForWrites {
    fn evaluate(&self, _spec: &ToolSpec, _call: &ToolCall) -> PolicyDecision {
        PolicyDecision::Ask
    }

    fn persistent_identity(&self) -> Option<String> {
        Some("ask-for-writes-v1".into())
    }
}
