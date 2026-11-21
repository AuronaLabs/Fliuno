//! Bounded A2A 1.0 JSON-RPC client. No core, provider, filesystem or agent policy.
//! Wire contract: a2aproject/A2A v1.0.1 specification, JSON-RPC binding.
//! Every mutating request is one-shot; callers own cancellation and recovery.
mod client;
mod oauth;
mod wire;
pub use client::{Client, MessageTarget, Options, PreparedMessage, validate_endpoint};
pub use oauth::ClientCredentials;
pub use wire::{
    ArtifactPayload, Message, SendResponse, StreamEvent, Task, TaskState, decode_raw_part,
};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("invalid A2A host configuration")]
    Config,
    #[error("A2A HTTP request failed")]
    Transport,
    #[error("A2A OAuth token acquisition failed")]
    Authentication,
    #[error("A2A Agent Card requires an unsupported extension")]
    UnsupportedExtension,
    #[error("A2A HTTP status {0}")]
    Http(u16),
    #[error("A2A response budget exceeded")]
    Budget,
    #[error("invalid A2A 1.0 response")]
    Protocol,
    #[error("A2A remote error {0}")]
    Remote(i64),
}
pub type Result<T> = std::result::Result<T, Error>;
