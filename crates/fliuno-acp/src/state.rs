use crate::{CancellationToken, ClientOptions, Error, Result, schema::*};
use std::{
    collections::{BTreeMap, HashMap},
    sync::Mutex,
};
use tokio::sync::{mpsc, oneshot};

pub(crate) const MAX_EVENT_BYTES: usize = 256 * 1024;
const MAX_EARLY_UPDATES: usize = 16;
pub(crate) const MAX_SESSIONS: usize = 64;
pub(crate) const MAX_PERMISSIONS: usize = 16;

/// Only per-request choices are supported. Persistent grants are never inferred.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionDecision {
    AllowOnce,
    Deny,
}

/// Live, connection-local events. They are not a durable Fliuno event log.
#[derive(Debug)]
pub enum Event {
    Update(Box<SessionNotification>),
    PermissionRequested {
        id: String,
        request: Box<RequestPermissionRequest>,
        expires_at_unix_ms: u64,
    },
    PermissionClosed {
        id: String,
        session_id: SessionId,
        outcome: RequestPermissionOutcome,
    },
    HostRequest {
        id: String,
        request: crate::HostRequest,
        expires_at_unix_ms: u64,
    },
    HostRequestClosed {
        id: String,
        session_id: SessionId,
    },
}

pub(crate) struct Session {
    pub active: Option<CancellationToken>,
}

pub(crate) struct PendingPermission {
    pub session_id: SessionId,
    pub options: Vec<PermissionOption>,
    pub response: oneshot::Sender<RequestPermissionOutcome>,
    pub expires_at: tokio::time::Instant,
}

#[derive(Default)]
pub(crate) struct State {
    pub sessions: HashMap<SessionId, Session>,
    // ACP permits notifications before session/new's response. Only one open is
    // in flight; buffer a small bounded set until the returned ID is validated.
    pub opening: Option<Vec<SessionNotification>>,
    pub pending: BTreeMap<String, PendingPermission>,
    pub host_pending: BTreeMap<String, crate::filesystem::PendingHostRequest>,
    pub failure: Option<Error>,
}

pub(crate) struct Shared {
    pub state: Mutex<State>,
    pub events: mpsc::Sender<Event>,
    pub stop: CancellationToken,
    pub finished: CancellationToken,
    pub options: ClientOptions,
}

impl Shared {
    pub fn fail(&self, error: Error) {
        let mut state = self.state.lock().unwrap();
        state.failure.get_or_insert(error);
        state.pending.clear();
        state.host_pending.clear();
        self.stop.cancel();
    }

    pub fn check(&self) -> Result<()> {
        if self.stop.is_cancelled() {
            return Err(self.failure());
        }
        Ok(())
    }

    pub fn failure(&self) -> Error {
        self.state
            .lock()
            .unwrap()
            .failure
            .clone()
            .unwrap_or(Error::Closed)
    }

    pub fn emit(&self, event: Event) -> Result<()> {
        if self.events.try_send(event).is_err() {
            self.fail(Error::EventOverflow);
            return Err(Error::EventOverflow);
        }
        Ok(())
    }

    pub fn update(&self, notification: SessionNotification) -> Result<()> {
        self.check()?;
        if serde_json::to_vec(&notification)
            .map_err(|_| Error::Protocol("cannot encode session update"))?
            .len()
            > MAX_EVENT_BYTES
        {
            self.fail(Error::Protocol("session update exceeds 256 KiB"));
            return Err(self.failure());
        }
        {
            let mut state = self.state.lock().unwrap();
            if !state.sessions.contains_key(&notification.session_id) {
                if let Some(early) = &mut state.opening
                    && early.len() < MAX_EARLY_UPDATES
                {
                    early.push(notification);
                    return Ok(());
                }
                drop(state);
                self.fail(Error::Protocol("update for unknown session"));
                return Err(self.failure());
            }
        }
        self.emit(Event::Update(Box::new(notification)))
    }

    pub fn cancel_permissions(&self, session_id: &SessionId) {
        let mut state = self.state.lock().unwrap();
        state.pending.retain(|_, p| &p.session_id != session_id);
        state
            .host_pending
            .retain(|_, p| &p.session_id != session_id);
        // Dropping the senders makes their waiters return Cancelled.
    }
}

/// A dropped prompt future has an unknown remote outcome. Close its connection
/// rather than letting a caller overlap a new prompt with an abandoned one.
pub(crate) struct PromptGuard {
    pub shared: std::sync::Arc<Shared>,
    pub session_id: SessionId,
    pub finished: bool,
}
impl Drop for PromptGuard {
    fn drop(&mut self) {
        {
            let mut state = self.shared.state.lock().unwrap();
            if let Some(session) = state.sessions.get_mut(&self.session_id) {
                session.active = None;
            }
            state.pending.retain(|_, p| p.session_id != self.session_id);
            state
                .host_pending
                .retain(|_, p| p.session_id != self.session_id);
        }
        if !self.finished {
            self.shared.fail(Error::OutcomeUnknown);
        }
    }
}

pub(crate) struct OpenGuard {
    pub shared: std::sync::Arc<Shared>,
    pub finished: bool,
}
impl Drop for OpenGuard {
    fn drop(&mut self) {
        if !self.finished {
            self.shared.state.lock().unwrap().opening = None;
            self.shared.fail(Error::OutcomeUnknown);
        }
    }
}
