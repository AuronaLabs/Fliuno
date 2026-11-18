use crate::ToolSpec;
use fliuno_protocol::ToolCall;

pub enum PolicyDecision {
    Ask,
    Deny(String),
}

/// Evaluates host-defined metadata, never model-supplied effect claims.
pub trait PolicyEngine: Send {
    fn evaluate(&self, spec: &ToolSpec, call: &ToolCall) -> PolicyDecision;
}

pub struct AskForWrites;

impl PolicyEngine for AskForWrites {
    fn evaluate(&self, _spec: &ToolSpec, _call: &ToolCall) -> PolicyDecision {
        PolicyDecision::Ask
    }
}
