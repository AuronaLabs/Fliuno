//! PTY supervisors, local IPC, bounded durable output and approval adapters.
//! The terminal service survives app-server connections; it never owns tasks,
//! model credentials, UI state or provider logic.
mod archive;
mod config;
#[cfg(feature = "test-fixtures")]
pub mod fixture;
mod ipc;
pub mod protocol;
mod registry;
mod service;
mod spool;
mod subscription;
mod tools;
pub mod worker;
pub use config::{Config, Isolation, PresetConfig, RetentionConfig};
pub use service::Service;
pub use subscription::Subscription;
pub use tools::register_config;
