//! A single catalog worker keeps ACP process startup and reads off the RPC loop.
use crossbeam_channel::{Sender, TrySendError, bounded};
use fliuno_core::{CancellationToken, PreparedAgentSessionList};
use fliuno_protocol::{AgentSessionListAccepted, AgentSessionListState};
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
    thread::{self, JoinHandle},
};

pub(super) const QUEUE_CAPACITY: usize = 4;
pub(super) const MAX_RETAINED: usize = 16;
type Request = (String, PreparedAgentSessionList);
type Replies = VecDeque<(String, Option<Arc<AgentSessionListState>>)>;

pub(super) struct Catalog {
    sender: Option<Sender<Request>>,
    replies: Arc<Mutex<Replies>>,
    cancellation: CancellationToken,
    worker: Option<JoinHandle<()>>,
}

impl Catalog {
    pub(super) fn new() -> Self {
        let (sender, requests) = bounded::<Request>(QUEUE_CAPACITY);
        let replies = Arc::new(Mutex::new(Replies::with_capacity(MAX_RETAINED)));
        let cancellation = CancellationToken::new();
        let results = Arc::clone(&replies);
        let stop = cancellation.clone();
        let worker = thread::Builder::new()
            .name("fliuno-agent-catalog".into())
            .spawn(move || {
                while let Ok((id, work)) = requests.recv() {
                    if stop.is_cancelled() {
                        break;
                    }
                    let result = work.execute(&stop);
                    let mut results = results.lock().unwrap();
                    if let Some((_, entry)) = results.iter_mut().find(|(key, _)| key == &id) {
                        *entry = Some(Arc::new(result));
                    }
                }
            })
            .expect("cannot start ACP catalog worker");
        Self {
            sender: Some(sender),
            replies,
            cancellation,
            worker: Some(worker),
        }
    }

    pub(super) fn start(
        &self,
        work: PreparedAgentSessionList,
    ) -> Result<AgentSessionListAccepted, super::RpcError> {
        let id = uuid::Uuid::new_v4().to_string();
        let mut replies = self.replies.lock().unwrap();
        // Hold the reply lock until the slot exists, even if the worker wins
        // the race to finish. A rejected request does not evict a result.
        self.sender
            .as_ref()
            .unwrap()
            .try_send((id.clone(), work))
            .map_err(|error| match error {
                TrySendError::Full(_) => super::RpcError {
                    code: -32009,
                    message: "agent session list queue is full".into(),
                },
                TrySendError::Disconnected(_) => super::RpcError {
                    code: -32603,
                    message: "agent session list worker stopped".into(),
                },
            })?;
        if replies.len() == MAX_RETAINED {
            // One worker and four queued requests leave at least one completed slot.
            let index = replies
                .iter()
                .position(|(_, state)| state.is_some())
                .unwrap();
            replies.remove(index);
        }
        replies.push_back((id.clone(), None));
        Ok(AgentSessionListAccepted { request_id: id })
    }

    pub(super) fn get(&self, id: &str) -> Option<serde_json::Value> {
        let state = self
            .replies
            .lock()
            .unwrap()
            .iter()
            .find_map(|(key, state)| (id == key).then(|| state.clone()))?;
        Some(serde_json::json!(
            state.as_deref().unwrap_or(&AgentSessionListState::Pending)
        ))
    }
}

impl Drop for Catalog {
    fn drop(&mut self) {
        self.cancellation.cancel();
        drop(self.sender.take());
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}
