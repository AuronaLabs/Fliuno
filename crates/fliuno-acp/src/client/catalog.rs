//! One bounded, read-only session catalog page on the negotiated peer.
use super::*;
use std::collections::HashSet;

const MAX_PAGE_ITEMS: usize = 128;
const MAX_CURSOR_BYTES: usize = 4096;

impl Client {
    pub fn supports_session_listing(&self) -> bool {
        self.initialized()
            .agent_capabilities
            .session_capabilities
            .list
            .is_some()
    }

    /// Fetch one page. The caller may explicitly request a subsequent page
    /// with its cursor. Discovered IDs are descriptors, not opened sessions.
    /// An isolated peer always receives a filter inside its private directory.
    pub async fn list_sessions(
        &self,
        cwd: Option<&Path>,
        cursor: Option<&str>,
    ) -> Result<ListSessionsResponse> {
        let shared = &self.inner.shared;
        shared.check()?;
        if !self.supports_session_listing() {
            return Err(Error::Unsupported("session/list"));
        }
        if cursor.is_some_and(|cursor| cursor.len() > MAX_CURSOR_BYTES) {
            return Err(Error::InvalidInput("session cursor exceeds 4096 bytes"));
        }
        let cwd = cwd.or_else(|| self.private_directory());
        if let Some(cwd) = cwd {
            if !cwd.is_absolute() {
                return Err(Error::InvalidInput(
                    "session filter must be an absolute path",
                ));
            }
            self.validate_private_cwd(cwd)?;
        }
        let _request = self.inner.catalog.try_lock().map_err(|_| Error::Busy)?;
        let request = ListSessionsRequest::new()
            .cwd(cwd.map(Path::to_path_buf))
            .cursor(cursor.map(str::to_owned));
        let page = tokio::select! {
            biased;
            _ = shared.stop.cancelled() => return Err(shared.failure()),
            response = tokio::time::timeout(
                shared.options.request_timeout,
                self.inner.connection.send_request(request).block_task(),
            ) => response.map_err(|_| Error::OutcomeUnknown)?
                .map_err(|error| Error::Remote(error.code.value()))?,
        };
        validate_page(&page, cursor)?;
        Ok(page)
    }
}

fn validate_page(page: &ListSessionsResponse, cursor: Option<&str>) -> Result<()> {
    if page.sessions.len() > MAX_PAGE_ITEMS
        || page
            .next_cursor
            .as_deref()
            .is_some_and(|next| next.len() > MAX_CURSOR_BYTES || Some(next) == cursor)
    {
        return Err(Error::Protocol("invalid session list page"));
    }
    let mut ids = HashSet::with_capacity(page.sessions.len());
    for session in &page.sessions {
        if session.session_id.0.is_empty()
            || session.session_id.0.len() > 256
            || !ids.insert(&session.session_id)
            || !session.cwd.is_absolute()
            || session.additional_directories.len() > 32
            || session
                .additional_directories
                .iter()
                .any(|root| !root.is_absolute())
        {
            return Err(Error::Protocol("invalid session list entry"));
        }
    }
    Ok(())
}
