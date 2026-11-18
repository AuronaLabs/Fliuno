//! Session-bound, expiring permission requests. Waiting runs outside SDK dispatch.
use crate::{Error, Event, PermissionDecision, Result, schema::*, state::*};
use std::{collections::HashSet, sync::Arc};
use tokio::sync::oneshot;
use uuid::Uuid;

impl Shared {
    pub fn respond_permission(
        &self,
        session_id: &SessionId,
        id: &str,
        decision: PermissionDecision,
    ) -> Result<()> {
        self.check()?;
        let mut state = self.state.lock().unwrap();
        let pending = state.pending.get(id).ok_or(Error::NotFound)?;
        if &pending.session_id != session_id {
            return Err(Error::NotFound);
        }
        if pending.expires_at <= tokio::time::Instant::now()
            || state
                .sessions
                .get(session_id)
                .and_then(|s| s.active.as_ref())
                .is_none_or(|turn| turn.is_cancelled())
        {
            state.pending.remove(id);
            return Err(Error::NotFound);
        }
        let kind = match decision {
            PermissionDecision::AllowOnce => PermissionOptionKind::AllowOnce,
            PermissionDecision::Deny => PermissionOptionKind::RejectOnce,
        };
        let outcome = match pending.options.iter().find(|option| option.kind == kind) {
            Some(option) => RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(
                option.option_id.clone(),
            )),
            None if decision == PermissionDecision::Deny => RequestPermissionOutcome::Cancelled,
            None => return Err(Error::InvalidInput("peer offered no allow_once option")),
        };
        state
            .pending
            .remove(id)
            .unwrap()
            .response
            .send(outcome)
            .map_err(|_| Error::NotFound)
    }
}

pub(crate) struct Waiter {
    shared: Arc<Shared>,
    id: String,
    session_id: SessionId,
    receiver: oneshot::Receiver<RequestPermissionOutcome>,
    turn_cancel: crate::CancellationToken,
    expires_at: tokio::time::Instant,
}

// Reserve capacity in dispatch, before spawning any waiter. A malicious peer
// cannot enqueue an unbounded number of suspended permission-handler tasks.
pub(crate) fn prepare(shared: Arc<Shared>, request: RequestPermissionRequest) -> Option<Waiter> {
    if shared.check().is_err()
        || request.options.len() > 16
        || request.options.is_empty()
        || crate::json_size::json_size(&request).map_or(true, |bytes| bytes > MAX_EVENT_BYTES)
    {
        return None;
    }
    let mut ids = HashSet::new();
    if request
        .options
        .iter()
        .any(|o| o.option_id.0.is_empty() || !ids.insert(o.option_id.clone()))
    {
        return None;
    }
    let id = format!("permission_{}", Uuid::new_v4());
    let (response, receiver) = oneshot::channel();
    let session_id = request.session_id.clone();
    let expires_at = tokio::time::Instant::now() + shared.options.permission_timeout;
    let expires_at_unix_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .saturating_add(shared.options.permission_timeout)
        .as_millis() as u64;
    let turn_cancel = {
        let mut state = shared.state.lock().unwrap();
        let cancel = state
            .sessions
            .get(&session_id)
            .and_then(|s| s.active.clone())
            .filter(|cancel| !cancel.is_cancelled())?;
        if state.pending.len() >= MAX_PERMISSIONS {
            return None;
        }
        state.pending.insert(
            id.clone(),
            PendingPermission {
                session_id: session_id.clone(),
                options: request.options.clone(),
                response,
                expires_at,
            },
        );
        cancel
    };
    if shared
        .emit(Event::PermissionRequested {
            id: id.clone(),
            request: Box::new(request),
            expires_at_unix_ms,
        })
        .is_err()
    {
        return None;
    }
    Some(Waiter {
        shared,
        id,
        session_id,
        receiver,
        turn_cancel,
        expires_at,
    })
}

impl Waiter {
    pub async fn resolve(mut self) -> RequestPermissionOutcome {
        let cancelled = RequestPermissionOutcome::Cancelled;
        let outcome = tokio::select! {
            biased;
            _ = self.shared.stop.cancelled() => cancelled,
            _ = self.turn_cancel.cancelled() => cancelled,
            _ = tokio::time::sleep_until(self.expires_at) => cancelled,
            result = &mut self.receiver => result.unwrap_or(cancelled),
        };
        self.shared.state.lock().unwrap().pending.remove(&self.id);
        let _ = self.shared.emit(Event::PermissionClosed {
            id: self.id.clone(),
            session_id: self.session_id.clone(),
            outcome: outcome.clone(),
        });
        outcome
    }
}

impl Drop for Waiter {
    fn drop(&mut self) {
        self.shared.state.lock().unwrap().pending.remove(&self.id);
    }
}
