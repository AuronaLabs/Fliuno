use crate::{CancellationToken, ClientOptions, Error, Result, schema::*};
use std::{
    collections::{BTreeMap, HashMap},
    sync::{Arc, Mutex},
};
use tokio::sync::{mpsc, oneshot};

pub(crate) const MAX_EVENT_BYTES: usize = 256 * 1024;
const MAX_EARLY_UPDATES: usize = 16;
const MAX_SESSIONS: usize = 64;
const MAX_HISTORY_UPDATES: usize = 1024;
const MAX_HISTORY_BYTES: usize = 16 * 1024 * 1024;
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
    /// Provisional replay from `session/load`. Audience code should commit it
    /// to its transcript only when that operation succeeds.
    SessionHistory(Box<SessionNotification>),
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
    opening: Option<Opening>,
    pub closing: Option<SessionId>,
    pub pending: BTreeMap<String, PendingPermission>,
    pub host_pending: BTreeMap<String, crate::filesystem::PendingHostRequest>,
    pub failure: Option<Error>,
}

pub(crate) enum Opening {
    // Buffer updates until the ID chosen by the peer can be checked.
    New(Vec<SessionNotification>),
    // The caller already selected the ID, so its history can be streamed.
    Load {
        id: SessionId,
        updates: usize,
        bytes: usize,
    },
    // Resume restores context on the peer without replaying session updates.
    Resume(SessionId),
}

impl Opening {
    pub(crate) fn load(id: SessionId) -> Self {
        Self::Load {
            id,
            updates: 0,
            bytes: 0,
        }
    }
}

pub(crate) fn valid_session_id(id: &SessionId) -> bool {
    !id.0.is_empty() && id.0.len() <= 256
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
        self.fail_locked(&mut self.state.lock().unwrap(), error);
    }

    fn fail_locked(&self, state: &mut State, error: Error) -> Error {
        let failure = state.failure.get_or_insert(error).clone();
        state.pending.clear();
        state.host_pending.clear();
        self.stop.cancel();
        failure
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
        let size = serde_json::to_vec(&notification)
            .map_err(|_| Error::Protocol("cannot encode session update"))?
            .len();
        if size > MAX_EVENT_BYTES {
            self.fail(Error::Protocol("session update exceeds 256 KiB"));
            return Err(self.failure());
        }
        let mut state = self.state.lock().unwrap();
        if state.sessions.contains_key(&notification.session_id) {
            drop(state);
            return self.emit(Event::Update(Box::new(notification)));
        }
        let error = match &mut state.opening {
            Some(Opening::New(early)) if early.len() < MAX_EARLY_UPDATES => {
                early.push(notification);
                return Ok(());
            }
            Some(Opening::Load { id, updates, bytes }) if *id == notification.session_id => {
                *updates += 1;
                *bytes += size;
                if *updates > MAX_HISTORY_UPDATES || *bytes > MAX_HISTORY_BYTES {
                    Error::Protocol("session load history exceeds budget")
                } else if self
                    .events
                    .try_send(Event::SessionHistory(Box::new(notification)))
                    .is_err()
                {
                    Error::EventOverflow
                } else {
                    return Ok(());
                }
            }
            _ => Error::Protocol("update for unknown session"),
        };
        Err(self.fail_locked(&mut state, error))
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
    shared: Arc<Shared>,
    finished: bool,
}

impl OpenGuard {
    pub(crate) fn begin(shared: &Arc<Shared>, opening: Opening) -> Result<Self> {
        let mut state = shared.state.lock().unwrap();
        if shared.stop.is_cancelled() {
            return Err(state.failure.clone().unwrap_or(Error::Closed));
        }
        if state.opening.is_some()
            || state.sessions.len() >= MAX_SESSIONS
            || matches!(&opening, Opening::Load { id, .. } | Opening::Resume(id) if state.sessions.contains_key(id))
        {
            return Err(Error::Busy);
        }
        state.opening = Some(opening);
        Ok(Self {
            shared: Arc::clone(shared),
            finished: false,
        })
    }

    pub(crate) fn finish(mut self, session_id: &SessionId) -> Result<()> {
        let mut state = self.shared.state.lock().unwrap();
        if self.shared.stop.is_cancelled() {
            return Err(state.failure.clone().unwrap_or(Error::Closed));
        }
        if !valid_session_id(session_id) || state.sessions.contains_key(session_id) {
            return Err(self
                .shared
                .fail_locked(&mut state, Error::Protocol("invalid or reused session ID")));
        }
        let early = match state.opening.take() {
            Some(Opening::New(early)) if early.iter().all(|n| &n.session_id == session_id) => early,
            Some(Opening::Load { id, .. } | Opening::Resume(id)) if &id == session_id => Vec::new(),
            _ => {
                return Err(self.shared.fail_locked(
                    &mut state,
                    Error::Protocol("early update for wrong session"),
                ));
            }
        };
        state
            .sessions
            .insert(session_id.clone(), Session { active: None });
        // Flush before later notifications can observe the new session entry.
        for notification in early {
            if self
                .shared
                .events
                .try_send(Event::Update(Box::new(notification)))
                .is_err()
            {
                return Err(self.shared.fail_locked(&mut state, Error::EventOverflow));
            }
        }
        self.finished = true;
        Ok(())
    }
}

impl Drop for OpenGuard {
    fn drop(&mut self) {
        if !self.finished {
            let mut state = self.shared.state.lock().unwrap();
            state.opening = None;
            self.shared.fail_locked(&mut state, Error::OutcomeUnknown);
        }
    }
}

/// A close may already have been applied if its acknowledgement was lost.
pub(crate) struct CloseGuard {
    shared: Arc<Shared>,
    finished: bool,
}

impl CloseGuard {
    pub(crate) fn begin(shared: &Arc<Shared>, session_id: &SessionId) -> Result<Self> {
        let mut state = shared.state.lock().unwrap();
        if shared.stop.is_cancelled() {
            return Err(state.failure.clone().unwrap_or(Error::Closed));
        }
        let session = state.sessions.get(session_id).ok_or(Error::NotFound)?;
        if state.closing.is_some() || session.active.is_some() {
            return Err(Error::Busy);
        }
        state.closing = Some(session_id.clone());
        Ok(Self {
            shared: Arc::clone(shared),
            finished: false,
        })
    }

    pub(crate) fn finish(mut self) -> Result<()> {
        let mut state = self.shared.state.lock().unwrap();
        if self.shared.stop.is_cancelled() {
            return Err(state.failure.clone().unwrap_or(Error::Closed));
        }
        let id = state.closing.take().expect("close guard owns session");
        state.sessions.remove(&id);
        self.finished = true;
        Ok(())
    }
}

impl Drop for CloseGuard {
    fn drop(&mut self) {
        if !self.finished {
            let mut state = self.shared.state.lock().unwrap();
            state.closing = None;
            self.shared.fail_locked(&mut state, Error::OutcomeUnknown);
        }
    }
}
