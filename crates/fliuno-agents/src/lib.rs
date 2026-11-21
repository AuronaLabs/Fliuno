//! Host-configured external-agent backends. Core does not depend on wire SDKs.
mod a2a;
mod acp;
mod filesystem;
pub use a2a::{A2aAgentConfig, A2aConfig, A2aOAuthConfig, register_a2a_config};
pub use acp::{AcpAgentConfig, AgentsConfig, Isolation, register_config};

#[cfg(test)]
mod lifecycle_tests {
    use fliuno_protocol::AgentStopReason;

    #[test]
    fn acp_and_a2a_share_completion_and_cancellation_reasons() {
        for (a2a_state, acp_reason, expected) in [
            (
                fliuno_a2a::TaskState::Completed,
                fliuno_acp::schema::StopReason::EndTurn,
                AgentStopReason::EndTurn,
            ),
            (
                fliuno_a2a::TaskState::Canceled,
                fliuno_acp::schema::StopReason::Cancelled,
                AgentStopReason::Cancelled,
            ),
        ] {
            assert_eq!(super::a2a::stop_reason(a2a_state), Some(expected));
            assert_eq!(super::acp::stop_reason(acp_reason), Some(expected));
        }
    }
}
