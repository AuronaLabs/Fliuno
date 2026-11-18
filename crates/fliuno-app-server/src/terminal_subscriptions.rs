//! Connection-local subscription lifecycle and bounded, fair notification queues.
//! Background readers never write stdout or wait for a slow application client.
use crossbeam_channel::{Receiver, Sender};
use fliuno_protocol::{
    TerminalOutput, TerminalOutputNotification, TerminalSubscribeParams, TerminalSubscription,
    TerminalSubscriptionEndReason as Reason, TerminalSubscriptionEnded,
};
use std::{
    collections::{BTreeMap, VecDeque},
    sync::{Arc, Mutex},
};
use tokio::{runtime::Runtime, task::AbortHandle};

pub(crate) const MAX_SUBSCRIPTIONS: usize = 8;
pub(crate) const PAGES_PER_SUBSCRIPTION: usize = 8;

struct Slot {
    identity: TerminalSubscription,
    pages: VecDeque<TerminalOutput>,
    end: Option<Reason>,
    task: Option<AbortHandle>,
}
#[derive(Default)]
struct Queues {
    slots: BTreeMap<String, Slot>,
    order: VecDeque<String>,
}

pub(crate) struct Subscriptions {
    queues: Arc<Mutex<Queues>>,
    wake: Sender<()>,
    receiver: Option<Receiver<()>>,
    runtime: Option<Runtime>,
}

pub(crate) enum Notification {
    Output(TerminalOutputNotification),
    Ended(TerminalSubscriptionEnded),
}
impl Notification {
    pub fn into_rpc(self) -> serde_json::Value {
        match self {
            Self::Output(params) => {
                serde_json::json!({"jsonrpc":"2.0", "method":"notifications/terminalOutput", "params":params})
            }
            Self::Ended(params) => {
                serde_json::json!({"jsonrpc":"2.0", "method":"notifications/terminalSubscriptionEnded", "params":params})
            }
        }
    }
}

impl Subscriptions {
    pub fn new() -> Self {
        let (wake, receiver) = crossbeam_channel::bounded(1);
        Self {
            queues: Arc::default(),
            wake,
            receiver: Some(receiver),
            runtime: None,
        }
    }
    pub fn take_receiver(&mut self) -> Receiver<()> {
        self.receiver
            .take()
            .expect("terminal notification receiver already taken")
    }
    pub fn start(
        &mut self,
        service: &fliuno_terminal::Service,
        params: TerminalSubscribeParams,
    ) -> Result<TerminalSubscription, String> {
        if self
            .queues
            .lock()
            .map_err(|_| "terminal subscription lock poisoned")?
            .slots
            .len()
            >= MAX_SUBSCRIPTIONS
        {
            return Err("terminal subscription limit reached".into());
        }
        if self.runtime.is_none() {
            self.runtime = Some(
                tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(2)
                    .enable_all()
                    .build()
                    .map_err(|_| "cannot create terminal subscription runtime")?,
            );
        }
        let runtime = self.runtime.as_ref().expect("subscription runtime");
        let mut subscription = runtime.block_on(service.subscribe(
            &params.session_id,
            &params.terminal_id,
            params.after,
            params.limit,
        ))?;
        let identity = TerminalSubscription {
            subscription_id: uuid::Uuid::new_v4().to_string(),
            session_id: params.session_id,
            terminal_id: params.terminal_id,
            after: params.after,
        };
        let id = identity.subscription_id.clone();
        let mut queues = self
            .queues
            .lock()
            .map_err(|_| "terminal subscription lock poisoned")?;
        queues.order.push_back(id.clone());
        queues.slots.insert(
            id.clone(),
            Slot {
                identity: identity.clone(),
                pages: VecDeque::new(),
                end: None,
                task: None,
            },
        );
        let shared = self.queues.clone();
        let wake = self.wake.clone();
        let task_id = id.clone();
        let task = runtime.spawn(async move {
            loop {
                match subscription.next().await {
                    Ok(Some(page)) => {
                        let Ok(mut queues) = shared.lock() else {
                            return;
                        };
                        let Some(slot) = queues.slots.get_mut(&task_id) else {
                            return;
                        };
                        if !slot.push(page) {
                            let _ = wake.try_send(());
                            return;
                        }
                    }
                    result => {
                        let reason = if result.is_err() {
                            Reason::Failed
                        } else {
                            subscription.end_reason().unwrap_or(Reason::Failed)
                        };
                        let Ok(mut queues) = shared.lock() else {
                            return;
                        };
                        if let Some(slot) = queues.slots.get_mut(&task_id) {
                            slot.end = Some(reason);
                        }
                        let _ = wake.try_send(());
                        return;
                    }
                }
                let _ = wake.try_send(());
            }
        });
        queues.slots.get_mut(&id).expect("registered slot").task = Some(task.abort_handle());
        Ok(identity)
    }

    pub fn stop(&self, id: &str) -> Result<bool, String> {
        let mut queues = self
            .queues
            .lock()
            .map_err(|_| "terminal subscription lock poisoned")?;
        queues.order.retain(|value| value != id);
        if let Some(slot) = queues.slots.remove(id) {
            if let Some(task) = slot.task {
                task.abort()
            }
            return Ok(true);
        }
        Ok(false)
    }

    pub fn next(&self) -> Option<Notification> {
        let mut queues = self.queues.lock().ok()?;
        let notification = queues.next();
        // Coalesced wakeups with round-robin dequeue: one flooding terminal
        // cannot starve others or allocate an unbounded global channel.
        if queues
            .slots
            .values()
            .any(|slot| !slot.pages.is_empty() || slot.end.is_some())
        {
            let _ = self.wake.try_send(());
        }
        notification
    }
}

impl Slot {
    fn push(&mut self, page: TerminalOutput) -> bool {
        if self.pages.len() == PAGES_PER_SUBSCRIPTION {
            // Preserve queued pages, then emit the end marker. Its cursor is
            // computed when dequeued, so it describes exactly emitted bytes.
            self.end = Some(Reason::Overflow);
            return false;
        }
        self.pages.push_back(page);
        true
    }
}
impl Queues {
    fn next(&mut self) -> Option<Notification> {
        for _ in 0..self.order.len() {
            let id = self.order.pop_front()?;
            let slot = self.slots.get_mut(&id)?;
            if let Some(output) = slot.pages.pop_front() {
                slot.identity.after = output.next;
                self.order.push_back(id.clone());
                return Some(Notification::Output(TerminalOutputNotification {
                    subscription_id: id,
                    output,
                }));
            }
            if let Some(reason) = slot.end {
                let identity = self.slots.remove(&id)?.identity;
                return Some(Notification::Ended(TerminalSubscriptionEnded {
                    subscription_id: id,
                    session_id: identity.session_id,
                    terminal_id: identity.terminal_id,
                    after: identity.after,
                    reason,
                }));
            }
            self.order.push_back(id);
        }
        None
    }
}
impl Drop for Subscriptions {
    fn drop(&mut self) {
        if let Ok(mut queues) = self.queues.lock() {
            for (_, slot) in std::mem::take(&mut queues.slots) {
                if let Some(task) = slot.task {
                    task.abort()
                }
            }
        }
        if let Some(runtime) = self.runtime.take() {
            runtime.shutdown_background()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fliuno_protocol::{TerminalSnapshot, TerminalStatus};

    fn slot(id: &str) -> Slot {
        Slot {
            identity: TerminalSubscription {
                subscription_id: id.into(),
                session_id: "s".into(),
                terminal_id: "t".into(),
                after: 10,
            },
            pages: VecDeque::new(),
            end: None,
            task: None,
        }
    }
    fn page(from: u64) -> TerminalOutput {
        TerminalOutput {
            from,
            next: from + 1,
            gap: false,
            data_base64: "eA==".into(),
            snapshot: TerminalSnapshot {
                terminal_id: "t".into(),
                session_id: "s".into(),
                preset_id: "p".into(),
                status: TerminalStatus::Running,
                startup_error: None,
                supervisor_connected: true,
                rows: 24,
                cols: 80,
                exit_code: None,
                signal: None,
                start_offset: 0,
                end_offset: from + 1,
                input_bytes: 0,
                output_closed: false,
                expires_at_unix_ms: 0,
                archived_at_unix_ms: None,
            },
        }
    }
    #[test]
    fn terminal_subscription_overflow_preserves_queued_bytes_and_fair_recovery_cursors() {
        let mut queues = Queues::default();
        for id in ["slow", "other"] {
            queues.order.push_back(id.into());
            queues.slots.insert(id.into(), slot(id));
        }
        let slow = queues.slots.get_mut("slow").unwrap();
        for from in 10..10 + PAGES_PER_SUBSCRIPTION as u64 {
            assert!(slow.push(page(from)))
        }
        assert!(!slow.push(page(18)));
        assert_eq!(slow.pages.len(), PAGES_PER_SUBSCRIPTION);
        let other = queues.slots.get_mut("other").unwrap();
        assert!(other.push(page(10)));
        other.end = Some(Reason::Closed);
        for expected in ["slow", "other", "slow"] {
            let Some(Notification::Output(n)) = queues.next() else {
                panic!("missing output")
            };
            assert_eq!(n.subscription_id, expected);
        }
        let Some(Notification::Ended(end)) = queues.next() else {
            panic!("missing end")
        };
        assert_eq!(end.subscription_id, "other");
        assert_eq!(end.after, 11);
        assert_eq!(end.reason, Reason::Closed);
        while let Some(n) = queues.next() {
            if let Notification::Ended(end) = n {
                assert_eq!(end.subscription_id, "slow");
                assert_eq!(end.after, 18);
                assert_eq!(end.reason, Reason::Overflow);
                assert!(queues.slots.is_empty());
                return;
            }
        }
        panic!("overflow notification was lost");
    }
    #[test]
    fn unsubscribe_suppresses_queued_output_and_end_markers() {
        let subscriptions = Subscriptions::new();
        {
            let mut queues = subscriptions.queues.lock().unwrap();
            let mut slot = slot("a");
            slot.push(page(10));
            slot.end = Some(Reason::Overflow);
            queues.slots.insert("a".into(), slot);
            queues.order.push_back("a".into());
        }
        assert!(subscriptions.stop("a").unwrap());
        assert!(!subscriptions.stop("a").unwrap());
        assert!(subscriptions.next().is_none());
    }
}
