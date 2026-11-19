//! At most one wake is pending; the latest committed sequence carries the state.
use crossbeam_channel::{Receiver, Sender, bounded};
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering::Relaxed},
};

pub(super) struct WatermarkSender {
    wake: Sender<PendingEventWatermark>,
    latest: Arc<AtomicU64>,
}

/// A wake for the newest matching commit. Resolve it after receiving it to
/// include commits made while it was waiting; resolving rearms the channel.
pub struct PendingEventWatermark(Option<Arc<AtomicU64>>);

impl PendingEventWatermark {
    pub fn through_sequence(mut self) -> u64 {
        self.0.take().unwrap().swap(0, Relaxed)
    }
}

impl Drop for PendingEventWatermark {
    fn drop(&mut self) {
        // Dropping an unread wake must not prevent a later wake.
        if let Some(latest) = self.0.take() {
            latest.store(0, Relaxed);
        }
    }
}

impl WatermarkSender {
    pub(super) fn new() -> (Self, Receiver<PendingEventWatermark>) {
        let (wake, receiver) = bounded(1);
        (
            Self {
                wake,
                latest: Arc::new(AtomicU64::new(0)),
            },
            receiver,
        )
    }

    /// The hub serializes publishers. A nonzero old value means an existing
    /// wake covers this commit; a receiver resetting to zero rearms delivery.
    pub(super) fn publish(&self, through: u64) -> bool {
        debug_assert!(through > 0);
        self.latest.swap(through, Relaxed) != 0
            || self
                .wake
                .try_send(PendingEventWatermark(Some(Arc::clone(&self.latest))))
                .is_ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{thread, time::Duration};

    #[test]
    fn unread_and_in_flight_wakes_share_one_slot_and_rearm() {
        let (sender, receiver) = WatermarkSender::new();
        for sequence in 1..=1000 {
            assert!(sender.publish(sequence));
        }
        assert_eq!(receiver.len(), 1);
        let wake = receiver.try_recv().unwrap();
        assert!(sender.publish(1001));
        assert!(receiver.is_empty());
        assert_eq!(wake.through_sequence(), 1001);
        assert!(sender.publish(1002));
        drop(receiver.try_recv().unwrap());
        assert!(sender.publish(1003));
        assert_eq!(receiver.try_recv().unwrap().through_sequence(), 1003);
        drop(receiver);
        assert!(!sender.publish(1004));
    }

    #[test]
    fn producer_and_reader_races_deliver_the_final_watermark() {
        let (sender, receiver) = WatermarkSender::new();
        const COMMITS: u64 = 20_000;
        let producer = thread::spawn(move || {
            for sequence in 1..=COMMITS {
                assert!(sender.publish(sequence));
            }
        });
        let mut through = 0;
        while through < COMMITS {
            let next = receiver
                .recv_timeout(Duration::from_secs(5))
                .unwrap()
                .through_sequence();
            assert!(next > through);
            through = next;
        }
        producer.join().unwrap();
        assert!(matches!(
            receiver.try_recv(),
            Err(crossbeam_channel::TryRecvError::Disconnected)
        ));
    }
}
