//! Host filesystem requests are session-bound one-shot messages. This module
//! never opens paths; an external broker owns policy, consent and actual IO.
use crate::{Error, Event, Result, schema::*, state::Shared};
use std::sync::Arc;
use tokio::sync::oneshot;

const MAX_PENDING: usize = 8;
const MAX_REQUEST_BYTES: usize = 1024 * 1024;
const MAX_TEXT_BYTES: usize = 256 * 1024;

#[derive(Debug)]
pub enum HostRequest {
    Read(Box<ReadTextFileRequest>),
    Write(Box<WriteTextFileRequest>),
}

impl HostRequest {
    pub fn session_id(&self) -> &SessionId {
        match self {
            Self::Read(r) => &r.session_id,
            Self::Write(r) => &r.session_id,
        }
    }
    fn valid(&self) -> bool {
        let (path, encoded) = match self {
            Self::Read(r) => {
                if r.line == Some(0) || r.limit == Some(0) {
                    return false;
                }
                (&r.path, crate::json_size::json_size(r))
            }
            Self::Write(r) => {
                if r.content.len() > MAX_TEXT_BYTES {
                    return false;
                }
                (&r.path, crate::json_size::json_size(r))
            }
        };
        path.is_absolute()
            && path
                .to_str()
                .is_some_and(|s| s.len() <= 4096 && !s.chars().any(char::is_control))
            && encoded.is_ok_and(|bytes| bytes <= MAX_REQUEST_BYTES)
    }
}

#[derive(Debug)]
pub enum HostResponse {
    Read(String),
    Written,
    Denied,
    Failed,
}

pub(crate) struct PendingHostRequest {
    pub session_id: SessionId,
    is_read: bool,
    response: oneshot::Sender<HostResponse>,
    expires_at: tokio::time::Instant,
}

pub(crate) struct Waiter {
    shared: Arc<Shared>,
    id: String,
    session_id: SessionId,
    receiver: oneshot::Receiver<HostResponse>,
    cancel: crate::CancellationToken,
    expires_at: tokio::time::Instant,
}

pub(crate) fn prepare(shared: Arc<Shared>, request: HostRequest) -> Option<Waiter> {
    if !shared.options.filesystem || shared.check().is_err() || !request.valid() {
        return None;
    }
    let id = format!("host_{}", uuid::Uuid::new_v4());
    let session_id = request.session_id().clone();
    let is_read = matches!(&request, HostRequest::Read(_));
    let (response, receiver) = oneshot::channel();
    let expires_at = tokio::time::Instant::now() + shared.options.permission_timeout;
    let expires_at_unix_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .saturating_add(shared.options.permission_timeout)
        .as_millis() as u64;
    let cancel = {
        let mut state = shared.state.lock().unwrap();
        let cancel = state
            .sessions
            .get(&session_id)
            .and_then(|s| s.active.clone())
            .filter(|c| !c.is_cancelled())?;
        if state.host_pending.len() >= MAX_PENDING {
            return None;
        }
        state.host_pending.insert(
            id.clone(),
            PendingHostRequest {
                session_id: session_id.clone(),
                is_read,
                response,
                expires_at,
            },
        );
        cancel
    };
    shared
        .emit(Event::HostRequest {
            id: id.clone(),
            request,
            expires_at_unix_ms,
        })
        .ok()?;
    Some(Waiter {
        shared,
        id,
        session_id,
        receiver,
        cancel,
        expires_at,
    })
}

impl Shared {
    pub fn host_request_remaining(
        &self,
        session_id: &SessionId,
        id: &str,
    ) -> Result<std::time::Duration> {
        self.check()?;
        let state = self.state.lock().unwrap();
        let pending = state
            .host_pending
            .get(id)
            .filter(|p| &p.session_id == session_id)
            .ok_or(Error::NotFound)?;
        if state
            .sessions
            .get(session_id)
            .and_then(|s| s.active.as_ref())
            .is_none_or(|c| c.is_cancelled())
        {
            return Err(Error::NotFound);
        }
        pending
            .expires_at
            .checked_duration_since(tokio::time::Instant::now())
            .filter(|d| !d.is_zero())
            .ok_or(Error::NotFound)
    }
    pub fn respond_host_request(
        &self,
        session_id: &SessionId,
        id: &str,
        response: HostResponse,
    ) -> Result<()> {
        self.check()?;
        let mut state = self.state.lock().unwrap();
        let pending = state.host_pending.get(id).ok_or(Error::NotFound)?;
        if &pending.session_id != session_id {
            return Err(Error::NotFound);
        }
        if pending.expires_at <= tokio::time::Instant::now()
            || state
                .sessions
                .get(session_id)
                .and_then(|s| s.active.as_ref())
                .is_none_or(|c| c.is_cancelled())
        {
            state.host_pending.remove(id);
            return Err(Error::NotFound);
        }
        match &response {
            HostResponse::Read(text) if !pending.is_read || text.len() > MAX_TEXT_BYTES => {
                return Err(Error::InvalidInput("invalid host read result"));
            }
            HostResponse::Written if pending.is_read => {
                return Err(Error::InvalidInput("invalid host write result"));
            }
            _ => {}
        }
        state
            .host_pending
            .remove(id)
            .unwrap()
            .response
            .send(response)
            .map_err(|_| Error::NotFound)
    }
}

impl Waiter {
    pub async fn resolve(mut self) -> HostResponse {
        let result = tokio::select! {
            biased;
            _ = self.shared.stop.cancelled() => HostResponse::Denied,
            _ = self.cancel.cancelled() => HostResponse::Denied,
            _ = tokio::time::sleep_until(self.expires_at) => HostResponse::Denied,
            result = &mut self.receiver => result.unwrap_or(HostResponse::Denied),
        };
        self.shared
            .state
            .lock()
            .unwrap()
            .host_pending
            .remove(&self.id);
        let _ = self.shared.emit(Event::HostRequestClosed {
            id: self.id.clone(),
            session_id: self.session_id.clone(),
        });
        result
    }
}
impl Drop for Waiter {
    fn drop(&mut self) {
        self.shared
            .state
            .lock()
            .unwrap()
            .host_pending
            .remove(&self.id);
    }
}
