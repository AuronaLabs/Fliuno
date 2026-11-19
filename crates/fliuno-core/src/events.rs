//! Bounded in-process event fan-out; SQLite remains the replay source of truth.
use crate::{Error, Result};
use crossbeam_channel::{Receiver, Sender, bounded};
use fliuno_protocol::{Event, ModelProgress};
use std::ops::Deref;
use std::sync::{Arc, Mutex, Weak};

const MAX_SUBSCRIBERS: usize = 8;
const SUBSCRIBER_CAPACITY: usize = 64;

pub struct SubscriptionReceiver<T> {
    receiver: Receiver<T>,
    active: Arc<()>,
}

pub type EventReceiver = SubscriptionReceiver<Event>;
pub type EventWatermarkReceiver = SubscriptionReceiver<u64>;

pub struct ProgressReceiver {
    receiver: Receiver<ModelProgress>,
    _active: Arc<()>,
}

impl ProgressReceiver {
    pub fn into_parts(self) -> (Receiver<ModelProgress>, Arc<()>) {
        (self.receiver, self._active)
    }
}

impl Deref for ProgressReceiver {
    type Target = Receiver<ModelProgress>;

    fn deref(&self) -> &Self::Target {
        &self.receiver
    }
}

impl<T> Deref for SubscriptionReceiver<T> {
    type Target = Receiver<T>;

    fn deref(&self) -> &Self::Target {
        &self.receiver
    }
}

impl<T> SubscriptionReceiver<T> {
    pub fn into_parts(self) -> (Receiver<T>, Arc<()>) {
        (self.receiver, self.active)
    }
}

/// Select the tasks whose commits a subscription will observe.
#[derive(Clone)]
pub enum EventScope {
    All,
    Task(String),
    Session(String),
    Workspace(String),
}

enum EventSender {
    Full(Sender<Event>),
    Watermark(Sender<u64>),
}

struct EventSubscriber {
    sender: EventSender,
    active: Weak<()>,
    scope: EventScope,
}

impl EventSubscriber {
    fn accepts(&self, task_id: &str, session_id: &str, workspace_id: Option<&str>) -> bool {
        match &self.scope {
            EventScope::All => true,
            EventScope::Task(id) => id == task_id,
            EventScope::Session(id) => id == session_id,
            EventScope::Workspace(id) => Some(id.as_str()) == workspace_id,
        }
    }
}

#[derive(Clone)]
pub(crate) struct EventHub {
    subscribers: Arc<Mutex<Vec<EventSubscriber>>>,
}

impl EventHub {
    pub(crate) fn new() -> Self {
        Self {
            subscribers: Arc::new(Mutex::new(Vec::new())),
        }
    }

    pub(crate) fn subscribe(&self, scope: EventScope) -> Result<EventReceiver> {
        self.subscribe_with(scope, EventSender::Full)
    }

    pub(crate) fn subscribe_watermarks(&self, scope: EventScope) -> Result<EventWatermarkReceiver> {
        self.subscribe_with(scope, EventSender::Watermark)
    }

    fn subscribe_with<T>(
        &self,
        scope: EventScope,
        sender_for: impl FnOnce(Sender<T>) -> EventSender,
    ) -> Result<SubscriptionReceiver<T>> {
        let mut subscribers = self
            .subscribers
            .lock()
            .map_err(|_| Error::Conflict("event hub lock is poisoned".into()))?;
        subscribers.retain(|subscriber| subscriber.active.strong_count() > 0);
        if subscribers.len() >= MAX_SUBSCRIBERS {
            return Err(Error::Conflict(
                "maximum event subscriber count reached".into(),
            ));
        }
        let (sender, receiver) = bounded(SUBSCRIBER_CAPACITY);
        let active = Arc::new(());
        subscribers.push(EventSubscriber {
            sender: sender_for(sender),
            active: Arc::downgrade(&active),
            scope,
        });
        Ok(SubscriptionReceiver { receiver, active })
    }

    /// Materialize bodies only for matching full-event subscribers.
    pub(crate) fn publish(
        &self,
        task_id: &str,
        session_id: &str,
        sequences: &[u64],
        event_at: impl Fn(usize, u64) -> Event,
        lookup_workspace: impl FnOnce() -> Result<String>,
    ) {
        let Some(&through) = sequences.last() else {
            return;
        };
        let Ok(mut subscribers) = self.subscribers.lock() else {
            return;
        };
        subscribers.retain(|subscriber| subscriber.active.strong_count() > 0);
        let has_workspace_scope = subscribers
            .iter()
            .any(|subscriber| matches!(subscriber.scope, EventScope::Workspace(_)));
        let workspace = if has_workspace_scope {
            match lookup_workspace() {
                Ok(id) => Some(id),
                Err(_) => {
                    // A disconnected stream signals a gap that can be replayed.
                    subscribers
                        .retain(|subscriber| !matches!(subscriber.scope, EventScope::Workspace(_)));
                    None
                }
            }
        } else {
            None
        };
        let workspace = workspace.as_deref();
        subscribers.retain(|subscriber| {
            if !subscriber.accepts(task_id, session_id, workspace) {
                return true;
            }
            match &subscriber.sender {
                EventSender::Full(sender) => sequences
                    .iter()
                    .copied()
                    .enumerate()
                    .all(|(index, sequence)| sender.try_send(event_at(index, sequence)).is_ok()),
                EventSender::Watermark(sender) => sender.try_send(through).is_ok(),
            }
        });
    }
}

#[derive(Clone)]
pub(crate) struct ProgressHub {
    state: Arc<Mutex<ProgressState>>,
}

#[derive(Default)]
struct ProgressState {
    generation: u64,
    subscribers: Vec<ProgressSubscriber>,
}

#[derive(Clone)]
pub struct ProgressPublisher {
    hub: ProgressHub,
    generation: Option<u64>,
}

impl ProgressPublisher {
    pub(crate) fn new(hub: ProgressHub) -> Self {
        let generation = hub.state.lock().ok().and_then(|mut state| {
            state.generation = state.generation.checked_add(1)?;
            Some(state.generation)
        });
        Self { hub, generation }
    }

    pub fn publish(&self, progress: ModelProgress) {
        if let Some(generation) = self.generation {
            self.hub.publish(generation, progress);
        }
    }
}

struct ProgressSubscriber {
    sender: Sender<ModelProgress>,
    active: Weak<()>,
    after_generation: u64,
}

impl ProgressHub {
    pub(crate) fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(ProgressState::default())),
        }
    }

    pub(crate) fn subscribe(&self, new_steps_only: bool) -> Result<ProgressReceiver> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| Error::Conflict("progress hub lock is poisoned".into()))?;
        let after_generation = if new_steps_only { state.generation } else { 0 };
        state
            .subscribers
            .retain(|subscriber| subscriber.active.strong_count() > 0);
        if state.subscribers.len() >= MAX_SUBSCRIBERS {
            return Err(Error::Conflict(
                "maximum progress subscriber count reached".into(),
            ));
        }
        let (sender, receiver) = bounded(SUBSCRIBER_CAPACITY);
        let active = Arc::new(());
        state.subscribers.push(ProgressSubscriber {
            sender,
            active: Arc::downgrade(&active),
            after_generation,
        });
        Ok(ProgressReceiver {
            receiver,
            _active: active,
        })
    }

    fn publish(&self, generation: u64, progress: ModelProgress) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        if let [subscriber] = state.subscribers.as_slice() {
            if subscriber.active.strong_count() == 0
                || (generation > subscriber.after_generation
                    && subscriber.sender.try_send(progress).is_err())
            {
                state.subscribers.clear();
            }
            return;
        }
        state.subscribers.retain(|subscriber| {
            subscriber.active.strong_count() > 0
                && (generation <= subscriber.after_generation
                    || subscriber.sender.try_send(progress.clone()).is_ok())
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slow_subscriber_is_disconnected_without_blocking_event_commit() {
        let hub = EventHub::new();
        let receiver = hub.subscribe(EventScope::All).unwrap();
        let sequences = (1..=SUBSCRIBER_CAPACITY as u64 + 1).collect::<Vec<_>>();
        hub.publish(
            "task_test",
            "session_test",
            &sequences,
            |_, sequence| Event {
                sequence,
                task_id: "task_test".into(),
                data: fliuno_protocol::EventKind::TaskStarted,
            },
            || unreachable!(),
        );

        for sequence in 1..=SUBSCRIBER_CAPACITY {
            assert_eq!(receiver.recv().unwrap().sequence, sequence as u64);
        }
        assert!(receiver.recv().is_err());
    }

    #[test]
    fn event_payloads_are_only_materialized_for_matching_subscribers() {
        let hub = EventHub::new();
        let scoped = hub.subscribe(EventScope::Session("other".into())).unwrap();
        let copies = std::cell::Cell::new(0);
        let event_at = |_, sequence| {
            copies.set(copies.get() + 1);
            Event {
                sequence,
                task_id: "task_test".into(),
                data: fliuno_protocol::EventKind::TaskStarted,
            }
        };
        let watermark = hub
            .subscribe_watermarks(EventScope::Session("current".into()))
            .unwrap();
        hub.publish("task_test", "current", &[1, 2], event_at, || unreachable!());
        assert_eq!(copies.get(), 0);
        assert_eq!(watermark.try_recv().unwrap(), 2);
        assert!(watermark.is_empty());

        let global = hub.subscribe(EventScope::All).unwrap();
        let matching = hub
            .subscribe(EventScope::Session("current".into()))
            .unwrap();
        hub.publish("task_test", "current", &[3], event_at, || unreachable!());
        assert_eq!(copies.get(), 2);
        assert!(scoped.is_empty());
        assert_eq!(global.try_recv().unwrap().sequence, 3);
        assert_eq!(matching.try_recv().unwrap().sequence, 3);
        assert_eq!(watermark.try_recv().unwrap(), 3);
    }

    #[test]
    fn dropped_subscribers_release_capacity_without_waiting_for_an_event() {
        let hub = EventHub::new();
        let mut receivers: Vec<_> = (0..MAX_SUBSCRIBERS)
            .map(|_| hub.subscribe(EventScope::All).unwrap())
            .collect();

        drop(receivers.pop());

        let replacement = hub.subscribe(EventScope::All).unwrap();
        assert!(hub.subscribe(EventScope::All).is_err());
        drop(replacement);
    }

    #[test]
    fn slow_progress_subscriber_is_detached_without_blocking_provider() {
        let hub = ProgressHub::new();
        let receiver = hub.subscribe(false).unwrap();
        let publisher = ProgressPublisher::new(hub.clone());
        for _ in 0..=SUBSCRIBER_CAPACITY {
            publisher.publish(ModelProgress::TextDelta {
                task_id: "task_test".into(),
                step: 1,
                text: "x".into(),
            });
        }
        for _ in 0..SUBSCRIBER_CAPACITY {
            assert!(matches!(
                receiver.recv().unwrap(),
                ModelProgress::TextDelta { .. }
            ));
        }
        assert!(receiver.recv().is_err());
    }
}
