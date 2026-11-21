use crate::{Error, Result};
use std::time::Duration;

/// Bounded waits and in-memory delivery. No credentials or process commands.
#[derive(Clone, Debug)]
pub struct ClientOptions {
    pub request_timeout: Duration,
    pub prompt_timeout: Duration,
    pub permission_timeout: Duration,
    pub cancellation_grace: Duration,
    pub event_capacity: usize,
    /// Requires an active event consumer to authorize and execute each request.
    pub filesystem: bool,
    /// Requires an active host broker for bounded terminal requests.
    pub terminal: bool,
}

impl Default for ClientOptions {
    fn default() -> Self {
        Self {
            request_timeout: Duration::from_secs(30),
            prompt_timeout: Duration::from_secs(300),
            permission_timeout: Duration::from_secs(120),
            cancellation_grace: Duration::from_secs(2),
            event_capacity: 128,
            filesystem: false,
            terminal: false,
        }
    }
}

impl ClientOptions {
    pub(crate) fn validate(&self) -> Result<()> {
        for timeout in [
            self.request_timeout,
            self.prompt_timeout,
            self.permission_timeout,
            self.cancellation_grace,
        ] {
            if timeout.is_zero() || timeout > Duration::from_secs(3600) {
                return Err(Error::InvalidInput("timeouts must be within 1 ns..1 hour"));
            }
        }
        if !(1..=1024).contains(&self.event_capacity) {
            return Err(Error::InvalidInput("event capacity must be within 1..1024"));
        }
        Ok(())
    }
}
