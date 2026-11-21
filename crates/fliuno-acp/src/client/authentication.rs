use super::*;
use crate::state::AuthenticationGuard;

impl Client {
    /// Distinguish a host-selected protocol method from a terminal login.
    /// Duplicate IDs are ambiguous, even if both have the same method type.
    pub fn supports_protocol_authentication(&self, method: &str) -> bool {
        if method.is_empty() || method.len() > 128 || method.chars().any(char::is_control) {
            return false;
        }
        let mut matching = self
            .initialized()
            .auth_methods
            .iter()
            .filter(|item| item.id().0.as_ref() == method);
        matches!(matching.next(), Some(AuthMethod::Agent(_))) && matching.next().is_none()
    }

    /// Send only the method ID, once on a new connection. No session may open
    /// while authentication is in flight; cancellation retires the connection.
    pub async fn authenticate(&self, method: &str, cancellation: &CancellationToken) -> Result<()> {
        let shared = &self.inner.shared;
        shared.check()?;
        if !self.supports_protocol_authentication(method) {
            return Err(Error::Unsupported(
                "selected protocol authentication method",
            ));
        }
        if cancellation.is_cancelled() {
            return Err(Error::Cancelled);
        }
        let _catalog = self.inner.catalog.try_lock().map_err(|_| Error::Busy)?;
        let guard = AuthenticationGuard::begin(shared)?;
        let response = tokio::select! {
            biased;
            _ = shared.stop.cancelled() => return Err(shared.failure()),
            _ = cancellation.cancelled() => return Err(Error::Cancelled),
            result = tokio::time::timeout(shared.options.request_timeout, self.inner.connection
                .send_request(AuthenticateRequest::new(method.to_owned())).block_task()) => {
                result.map_err(|_| Error::OutcomeUnknown)?
            },
        };
        match response {
            Ok(_) => {
                guard.resolve();
                Ok(())
            }
            Err(error)
                if acp::is_connection_terminated(&error)
                    || acp::is_incoming_transport_closed(&error) =>
            {
                Err(Error::OutcomeUnknown)
            }
            Err(error) => {
                guard.resolve();
                Err(Error::Remote(error.code.value()))
            }
        }
    }
}
