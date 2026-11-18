//! Open a new or existing ACP session on an already negotiated connection.
use super::*;

impl Client {
    /// Opening is serialized and never retried implicitly.
    pub async fn new_session(&self, cwd: impl AsRef<Path>) -> Result<NewSessionResponse> {
        let shared = &self.inner.shared;
        shared.check()?;
        let cwd = self.checked_session_directory(cwd.as_ref())?;
        let opening = OpenGuard::begin(shared, Opening::New(Vec::new()))?;
        let response = tokio::select! {
            biased;
            _ = shared.stop.cancelled() => return Err(shared.failure()),
            response = tokio::time::timeout(
                shared.options.request_timeout,
                self.inner.connection.send_request(NewSessionRequest::new(cwd)).block_task(),
            ) => response.map_err(|_| Error::OutcomeUnknown)?
                .map_err(|error| Error::Remote(error.code.value()))?,
        };
        opening.finish(&response.session_id)?;
        Ok(response)
    }

    pub fn supports_session_loading(&self) -> bool {
        self.initialized().agent_capabilities.load_session
    }

    /// Load one explicit session without adding tools or workspace roots.
    /// Concurrently consume `Event::SessionHistory` from the event feed. Treat
    /// the received history as provisional until this request succeeds.
    pub async fn load_session(
        &self,
        session_id: &SessionId,
        cwd: impl AsRef<Path>,
    ) -> Result<LoadSessionResponse> {
        let shared = &self.inner.shared;
        shared.check()?;
        if !self.supports_session_loading() {
            return Err(Error::Unsupported("session/load"));
        }
        if !valid_session_id(session_id) {
            return Err(Error::InvalidInput("invalid session ID"));
        }
        let cwd = self.checked_session_directory(cwd.as_ref())?;
        let opening = OpenGuard::begin(shared, Opening::load(session_id.clone()))?;
        let response = tokio::select! {
            biased;
            _ = shared.stop.cancelled() => return Err(shared.failure()),
            response = tokio::time::timeout(
                shared.options.request_timeout,
                self.inner.connection.send_request(LoadSessionRequest::new(session_id.clone(), cwd)).block_task(),
            ) => response.map_err(|_| Error::OutcomeUnknown)?
                .map_err(|error| Error::Remote(error.code.value()))?,
        };
        opening.finish(session_id)?;
        Ok(response)
    }

    fn checked_session_directory<'a>(&self, cwd: &'a Path) -> Result<&'a Path> {
        if !cwd.is_absolute() || !cwd.is_dir() {
            return Err(Error::InvalidInput(
                "cwd must be an absolute existing directory",
            ));
        }
        self.validate_private_cwd(cwd)?;
        Ok(cwd)
    }
}
