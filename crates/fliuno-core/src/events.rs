//! Bounded in-process event fan-out; SQLite remains the replay source of truth.
use crate::{Error, Result};
use crossbeam_channel::{Receiver, Sender, TrySendError, bounded};
use fliuno_protocol::{Event, ModelProgress};
use std::ops::Deref;
use std::sync::{Arc, Mutex, Weak};

const MAX_SUBSCRIBERS: usize = 8;
const SUBSCRIBER_CAPACITY: usize = 64;

pub struct EventReceiver {
    receiver: Receiver<Event>,
    _active: Arc<()>,
}

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

impl Deref for EventReceiver {
    type Target = Receiver<Event>;

    fn deref(&self) -> &Self::Target {
        &self.receiver
    }
}

impl EventReceiver {
    pub fn into_parts(self) -> (Receiver<Event>, Arc<()>) {
        (self.receiver, self._active)
    }
}

struct EventSubscriber {
    sender: Sender<Event>,
    active: Weak<()>,
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

    pub(crate) fn subscribe(&self) -> Result<EventReceiver> {
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
            sender,
            active: Arc::downgrade(&active),
        });
        Ok(EventReceiver {
            receiver,
            _active: active,
        })
    }

    pub(crate) fn publish(&self, events: Vec<Event>) {
        if events.is_empty() {
            return;
        }
        let Ok(mut subscribers) = self.subscribers.lock() else {
            return;
        };
        if let [subscriber] = subscribers.as_slice() {
            // A stdio server normally has one subscriber. Move the committed
            // event directly into its queue, even for large tool payloads.
            if subscriber.active.strong_count() == 0
                || events
                    .into_iter()
                    .any(|event| subscriber.sender.try_send(event).is_err())
            {
                subscribers.clear();
            }
            return;
        }
        subscribers.retain(|subscriber| {
            if subscriber.active.strong_count() == 0 {
                return false;
            }
            for event in &events {
                match subscriber.sender.try_send(event.clone()) {
                    Ok(()) => {}
                    Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_)) => {
                        return false;
                    }
                }
            }
            true
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
        let receiver = hub.subscribe().unwrap();
        let events = (1..=SUBSCRIBER_CAPACITY + 1)
            .map(|sequence| Event {
                sequence: sequence as u64,
                task_id: "task_test".into(),
                data: fliuno_protocol::EventKind::TaskStarted,
            })
            .collect();

        hub.publish(events);

        for sequence in 1..=SUBSCRIBER_CAPACITY {
            assert_eq!(receiver.recv().unwrap().sequence, sequence as u64);
        }
        assert!(receiver.recv().is_err());
    }

    #[test]
    fn dropped_subscribers_release_capacity_without_waiting_for_an_event() {
        let hub = EventHub::new();
        let mut receivers: Vec<_> = (0..MAX_SUBSCRIBERS)
            .map(|_| hub.subscribe().unwrap())
            .collect();

        drop(receivers.pop());

        let replacement = hub.subscribe().unwrap();
        assert!(hub.subscribe().is_err());
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
