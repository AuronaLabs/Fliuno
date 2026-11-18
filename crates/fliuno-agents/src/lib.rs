//! Host-configured external-agent backends. Core does not depend on wire SDKs.
mod a2a;
mod acp;
mod filesystem;
pub use a2a::{A2aAgentConfig, A2aConfig, register_a2a_config};
pub use acp::{AcpAgentConfig, AgentsConfig, Isolation, register_config};
