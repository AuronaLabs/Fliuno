//! ACP v1 client boundary. Independent of Fliuno task storage and UI protocols.
//!
//! Sessions and permissions here are connection-local, not durable kernel tasks.
//! The client advertises no filesystem or terminal capabilities. Protocol consent
//! does not isolate an agent's own tools; use `connect_isolated` for a native
//! Windows agent, or supply a transport with host-managed authority and lifetime.
mod client;
mod config;
mod filesystem;
mod permissions;
mod state;
mod transport;

#[cfg(feature = "test-fixtures")]
#[doc(hidden)]
pub mod fixture;

pub use agent_client_protocol::v1::schema;
pub use client::{Client, Events};
pub use config::ClientOptions;
pub use filesystem::{HostRequest, HostResponse};
pub use state::{Event, PermissionDecision};
pub use tokio_util::sync::CancellationToken;

/// Errors intentionally omit peer error messages/data, which may contain secrets.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    #[error("invalid ACP input: {0}")]
    InvalidInput(&'static str),
    #[error("ACP connection is closed")]
    Closed,
    #[error("ACP session or connection is busy")]
    Busy,
    #[error("unknown ACP session or expired permission")]
    NotFound,
    #[error("ACP protocol violation: {0}")]
    Protocol(&'static str),
    #[error("ACP peer returned error code {0}")]
    Remote(i32),
    #[error("ACP operation timed out or was interrupted; remote result is unknown")]
    OutcomeUnknown,
    #[error("ACP event consumer fell behind or disconnected; connection stopped")]
    EventOverflow,
    #[error("ACP process startup or transport failed")]
    Transport,
    #[error("ACP operation was cancelled before sending")]
    Cancelled,
}

pub type Result<T> = std::result::Result<T, Error>;
