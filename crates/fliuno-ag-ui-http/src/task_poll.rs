//! Share one scoped sample between the active HTTP bodies on a connection.
use super::{Gateway, POLL_INTERVAL, Status, StatusCode, rpc_status};
use fliuno_protocol::TaskWatermarksParams;
use std::{
    collections::HashMap,
    io::{BufRead, Write},
    sync::{Arc, Mutex as SyncMutex},
};
use tokio::{sync::Mutex, time::Instant};

type TaskCounts = SyncMutex<HashMap<Arc<str>, usize>>;

#[derive(Default)]
pub(super) struct TaskPoll {
    watched: Arc<TaskCounts>,
    sample: Mutex<Option<Sample>>,
}

struct Sample {
    taken: Instant,
    by_task: HashMap<Arc<str>, Option<u64>>,
}

/// An HTTP body owns its registration, including when it is dropped mid-page.
pub(super) struct TaskWatch {
    task: Arc<str>,
    watched: Arc<TaskCounts>,
}

impl Drop for TaskWatch {
    fn drop(&mut self) {
        let mut watched = self.watched.lock().expect("task watch registry");
        if let Some(count) = watched.get_mut(&self.task) {
            *count -= 1;
            if *count == 0 {
                watched.remove(&self.task);
            }
        }
    }
}

impl TaskPoll {
    pub(super) fn watch(&self, task: String) -> TaskWatch {
        let task: Arc<str> = Arc::from(task);
        *self
            .watched
            .lock()
            .expect("task watch registry")
            .entry(task.clone())
            .or_default() += 1;
        TaskWatch {
            task,
            watched: self.watched.clone(),
        }
    }

    pub(super) async fn poll<R: BufRead + Send + 'static, W: Write + Send + 'static>(
        &self,
        gateway: &Gateway<R, W>,
        watch: &TaskWatch,
    ) -> Status<u64> {
        let mut cached = self.sample.lock().await;
        if let Some(sample) = cached.as_ref()
            && sample.taken.elapsed() < POLL_INTERVAL
            && let Some(sequence) = sample.by_task.get(&watch.task)
        {
            return sequence.ok_or(StatusCode::BAD_GATEWAY);
        }
        // A stream can join between samples. Refill if its task wasn't in the
        // cached batch; otherwise coalesce idle readers for the poll interval.
        // Drop the registry lock before the RPC so a disconnected body can unregister.
        let tasks: Vec<_> = self
            .watched
            .lock()
            .expect("task watch registry")
            .keys()
            .cloned()
            .collect();
        let task_ids = tasks.iter().map(|id| id.to_string()).collect();
        let result = gateway
            .rpc(move |client| {
                client
                    .task_event_watermarks(&TaskWatermarksParams { task_ids }, |_| {})
                    .map_err(rpc_status)
            })
            .await?;
        let by_task: HashMap<_, _> = tasks.into_iter().zip(result.through_sequences).collect();
        let sequence = by_task.get(&watch.task).copied().flatten();
        *cached = Some(Sample {
            taken: Instant::now(),
            by_task,
        });
        sequence.ok_or(StatusCode::BAD_GATEWAY)
    }
}
