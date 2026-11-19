//! Prepare one catalog read without holding the kernel lock during agent IO.
use super::*;

const MAX_CURSOR_BYTES: usize = 4096;
const MAX_PAGE_BYTES: usize = 256 * 1024;

pub struct PreparedAgentSessionList {
    backend: Arc<dyn AgentBackend>,
    cursor: Option<String>,
}

impl PreparedAgentSessionList {
    pub fn execute(self, cancellation: &CancellationToken) -> AgentSessionListState {
        use AgentSessionListFailure::{InvalidPage, Unavailable};
        let page = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            if cancellation.is_cancelled() {
                return Err(Unavailable);
            }
            let page = self
                .backend
                .list_agent_sessions(self.cursor.as_deref(), cancellation)?;
            if json_size(&page).map_err(|_| InvalidPage)? > MAX_PAGE_BYTES {
                return Err(InvalidPage);
            }
            Ok(page)
        }))
        .unwrap_or(Err(Unavailable));
        match page {
            Ok(page) => AgentSessionListState::Complete { page },
            Err(reason) => AgentSessionListState::Failed { reason },
        }
    }
}

impl Kernel {
    pub fn agent_session_listing_supported(&self) -> bool {
        self.agents
            .values()
            .any(|agent| agent.supports_agent_session_listing())
    }

    pub fn prepare_agent_session_list(
        &self,
        params: AgentSessionListParams,
    ) -> Result<PreparedAgentSessionList> {
        if params
            .cursor
            .as_ref()
            .is_some_and(|cursor| cursor.len() > MAX_CURSOR_BYTES)
        {
            return Err(Error::Invalid(
                "agent session cursor exceeds 4096 bytes".into(),
            ));
        }
        let backend = self
            .agents
            .get(&params.agent_id)
            .ok_or_else(|| Error::NotFound("agent".into()))?;
        if !backend.supports_agent_session_listing() {
            return Err(Error::Conflict(
                "agent session listing is not configured".into(),
            ));
        }
        Ok(PreparedAgentSessionList {
            backend: Arc::clone(backend),
            cursor: params.cursor,
        })
    }
}
