//! Bounded workers run independent task steps without holding database locks.
use crossbeam_channel::{Receiver, Sender, TrySendError, bounded};
use fliuno_core::{
    AgentObservation, AgentObserver, AgentOutcome, CancellationToken, Error, Kernel, ModelError,
    ModelStreamEvent, ModelTurn, PreparedModelStep, PreparedToolExecution, ProgressPublisher,
    ToolError, ToolOutput,
};
use fliuno_protocol::{ModelProgress, Task};
use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

pub(super) const QUEUE_CAPACITY: usize = 32;
pub(super) const WORKER_COUNT: usize = 4;
type Cancellation = CancellationToken;
type ActiveTasks = Arc<Mutex<HashMap<String, Cancellation>>>;
type ModelResponse = (Task, Result<ModelTurn, ModelError>);
type ToolResponse = (PreparedToolExecution, Result<ToolOutput, ToolError>);
type AgentResponse = (String, String, AgentOutcome);

enum ScheduledWork {
    Model,
    Agent,
    Tool(Box<PreparedToolExecution>),
    Stop,
}

enum ScheduledResponse {
    Model(Box<ModelResponse>),
    Tool(Box<ToolResponse>),
    Agent(Box<AgentResponse>),
}

struct ScheduledTask {
    id: String,
    cancellation: Cancellation,
    work: ScheduledWork,
}

pub(super) struct TaskScheduler {
    sender: Option<Sender<ScheduledTask>>,
    active: ActiveTasks,
    stopping: Arc<AtomicBool>,
    kernel: Arc<Mutex<Kernel>>,
    workers: Vec<JoinHandle<()>>,
}

impl TaskScheduler {
    pub(super) fn new(kernel: Arc<Mutex<Kernel>>) -> Self {
        Self::with_worker_count(kernel, WORKER_COUNT)
    }

    fn with_worker_count(kernel: Arc<Mutex<Kernel>>, worker_count: usize) -> Self {
        assert!(worker_count > 0, "scheduler requires at least one worker");
        let (sender, receiver) = bounded::<ScheduledTask>(QUEUE_CAPACITY);
        let active = Arc::new(Mutex::new(HashMap::new()));
        let stopping = Arc::new(AtomicBool::new(false));
        let workers = (0..worker_count)
            .map(|index| {
                let worker_kernel = Arc::clone(&kernel);
                let worker_receiver = receiver.clone();
                let worker_sender = sender.clone();
                let worker_active = Arc::clone(&active);
                let worker_stopping = Arc::clone(&stopping);
                thread::Builder::new()
                    .name(format!("fliuno-task-scheduler-{index}"))
                    .spawn(move || {
                        run_worker(
                            worker_receiver,
                            worker_sender,
                            worker_kernel,
                            worker_active,
                            worker_stopping,
                        )
                    })
                    .expect("cannot start task scheduler worker")
            })
            .collect();
        Self {
            sender: Some(sender),
            active,
            stopping,
            kernel,
            workers,
        }
    }

    pub(super) fn schedule(&self, task_id: &str) -> Result<(), &'static str> {
        self.schedule_work(task_id, ScheduledWork::Model)
    }

    pub(super) fn schedule_agent(&self, task_id: &str) -> Result<(), &'static str> {
        self.schedule_work(task_id, ScheduledWork::Agent)
    }

    pub(super) fn schedule_tool(
        &self,
        execution: Box<PreparedToolExecution>,
    ) -> Result<(), &'static str> {
        let task_id = execution.task_id().to_owned();
        self.schedule_work(&task_id, ScheduledWork::Tool(execution))
    }

    fn schedule_work(&self, task_id: &str, work: ScheduledWork) -> Result<(), &'static str> {
        let mut active = self
            .active
            .lock()
            .map_err(|_| "task scheduler lock is poisoned")?;
        if active.contains_key(task_id) {
            return Ok(());
        }
        let cancellation = CancellationToken::new();
        active.insert(task_id.into(), cancellation.clone());
        let job = ScheduledTask {
            id: task_id.into(),
            cancellation,
            work,
        };
        let sender = self
            .sender
            .as_ref()
            .ok_or("task scheduler is unavailable")?;
        if let Err(error) = sender.try_send(job) {
            active.remove(task_id);
            return Err(match error {
                TrySendError::Full(_) => "task scheduler queue is full",
                TrySendError::Disconnected(_) => "task scheduler is unavailable",
            });
        }
        Ok(())
    }

    pub(super) fn cancel(&self, task_id: &str) -> Result<(), &'static str> {
        if let Some(token) = self
            .active
            .lock()
            .map_err(|_| "task scheduler lock is poisoned")?
            .get(task_id)
        {
            token.cancel();
        }
        Ok(())
    }

    #[cfg(test)]
    pub(super) fn is_idle(&self) -> bool {
        self.active.lock().unwrap().is_empty()
    }
}

impl Drop for TaskScheduler {
    fn drop(&mut self) {
        self.stopping.store(true, Ordering::Release);
        if let Ok(active) = self.active.lock() {
            for token in active.values() {
                token.cancel();
            }
        }
        if let Some(sender) = self.sender.take() {
            for _ in 0..self.workers.len() {
                if sender
                    .try_send(ScheduledTask {
                        id: String::new(),
                        cancellation: CancellationToken::new(),
                        work: ScheduledWork::Stop,
                    })
                    .is_err()
                {
                    break;
                }
            }
        }
        // Joining prevents abandoned workers retaining the DB lock or running
        // queued requests; providers must observe cancellation to exit promptly.
        for worker in self.workers.drain(..) {
            if worker.join().is_err() {
                eprintln!("fliuno: task scheduler worker panicked");
            }
        }
        if let Ok(mut kernel) = self.kernel.lock()
            && let Err(error) = kernel.interrupt_running_tasks()
        {
            eprintln!("fliuno: cannot persist scheduler shutdown: {error}");
        }
    }
}

fn run_worker(
    receiver: Receiver<ScheduledTask>,
    sender: Sender<ScheduledTask>,
    kernel: Arc<Mutex<Kernel>>,
    active: ActiveTasks,
    stopping: Arc<AtomicBool>,
) {
    while let Ok(job) = receiver.recv() {
        if stopping.load(Ordering::Acquire) || matches!(&job.work, ScheduledWork::Stop) {
            break;
        }
        let ScheduledTask {
            id,
            cancellation,
            work,
        } = job;
        let response = match work {
            ScheduledWork::Model => run_model_step(&kernel, &id, &cancellation, &stopping)
                .map(|response| response.map(Box::new).map(ScheduledResponse::Model)),
            ScheduledWork::Tool(execution) => {
                run_tool_step(&kernel, *execution, &cancellation, &stopping)
                    .map(|response| response.map(Box::new).map(ScheduledResponse::Tool))
            }
            ScheduledWork::Agent => run_agent_step(&kernel, &id, &cancellation, &stopping)
                .map(|response| response.map(Box::new).map(ScheduledResponse::Agent)),
            ScheduledWork::Stop => unreachable!("stop jobs exit before dispatch"),
        };
        let Ok(mut kernel) = kernel.lock() else {
            eprintln!("fliuno: kernel lock is poisoned");
            break;
        };
        // Release the reservation before exposing another approval or step.
        if let Ok(mut active) = active.lock() {
            active.remove(&id);
        }
        if stopping.load(Ordering::Acquire) {
            break;
        }
        let result = match response {
            Ok(Some(ScheduledResponse::Agent(response))) => {
                let (task_id, run_id, outcome) = *response;
                kernel
                    .finish_agent_run(&task_id, &run_id, outcome)
                    .map(|_| ())
            }
            Ok(Some(ScheduledResponse::Model(response))) => {
                let (task, response) = *response;
                kernel.apply_model_response(task, response).map(|_| ())
            }
            Ok(Some(ScheduledResponse::Tool(response))) => {
                let (execution, response) = *response;
                match kernel.apply_tool_response(&execution, response) {
                    Ok(task) if task.status == fliuno_protocol::TaskStatus::Running => {
                        schedule_followup_model(&sender, &active, &mut kernel, &task)
                    }
                    Ok(_) => Ok(()),
                    Err(error) => Err(error),
                }
            }
            Ok(None) => Ok(()),
            Err(error) => Err(error),
        };
        if let Err(error) = result {
            eprintln!("fliuno: task scheduler: {error}");
        }
    }
}

struct PersistentAgentObserver {
    kernel: Arc<Mutex<Kernel>>,
    task_id: String,
    run_id: String,
}

impl AgentObserver for PersistentAgentObserver {
    fn observe_tool(
        &self,
        observation: fliuno_core::AgentToolObservation,
    ) -> fliuno_core::Result<()> {
        self.kernel
            .lock()
            .map_err(|_| Error::Conflict("kernel lock is poisoned".into()))?
            .observe_agent_tool(&self.task_id, &self.run_id, observation)
    }
    fn observe(&self, observation: AgentObservation) -> fliuno_core::Result<()> {
        self.kernel
            .lock()
            .map_err(|_| Error::Conflict("kernel lock is poisoned".into()))?
            .observe_agent(&self.task_id, &self.run_id, observation)
    }
}

fn run_agent_step(
    kernel: &Arc<Mutex<Kernel>>,
    task_id: &str,
    cancellation: &CancellationToken,
    stopping: &AtomicBool,
) -> fliuno_core::Result<Option<AgentResponse>> {
    if cancellation.is_cancelled() || stopping.load(Ordering::Acquire) {
        return Ok(None);
    }
    let prepared = kernel
        .lock()
        .map_err(|_| Error::Conflict("kernel lock is poisoned".into()))?
        .prepare_agent_run(task_id, cancellation)?;
    let Some(prepared) = prepared else {
        return Ok(None);
    };
    let observer = PersistentAgentObserver {
        kernel: Arc::clone(kernel),
        task_id: task_id.into(),
        run_id: prepared.request.run_id.clone(),
    };
    let outcome = if cancellation.is_cancelled() || stopping.load(Ordering::Acquire) {
        AgentOutcome::unknown()
    } else {
        prepared.execute(&observer, cancellation)
    };
    Ok(Some((observer.task_id, observer.run_id, outcome)))
}

fn run_model_step(
    kernel: &Arc<Mutex<Kernel>>,
    task_id: &str,
    cancellation: &CancellationToken,
    stopping: &AtomicBool,
) -> fliuno_core::Result<Option<ModelResponse>> {
    if cancellation.is_cancelled() || stopping.load(Ordering::Acquire) {
        return Ok(None);
    }
    let (prepared, provider, progress_publisher) = {
        let mut kernel = kernel
            .lock()
            .map_err(|_| Error::Conflict("kernel lock is poisoned".into()))?;
        (
            kernel.prepare_model_step(task_id)?,
            kernel.model_provider_handle(),
            kernel.model_progress_publisher(),
        )
    };
    let PreparedModelStep::Ready { task, tools } = prepared else {
        return Ok(None);
    };
    if cancellation.is_cancelled() || stopping.load(Ordering::Acquire) {
        return Ok(None);
    }
    let task_id = task.id.clone();
    let step = task.model_steps;
    let mut pending_text = String::new();
    let mut last_publish = Instant::now();
    let mut has_published_text = false;
    let mut emit_progress = |update| match update {
        ModelStreamEvent::TextDelta(text) => {
            pending_text.push_str(&text);
            if !has_published_text
                || pending_text.len() >= 1024
                || last_publish.elapsed() >= Duration::from_millis(30)
            {
                publish_text_progress(
                    &progress_publisher,
                    &task_id,
                    step,
                    std::mem::take(&mut pending_text),
                );
                last_publish = Instant::now();
                has_published_text = true;
            }
        }
        ModelStreamEvent::Reset => {
            if !pending_text.is_empty() {
                publish_text_progress(
                    &progress_publisher,
                    &task_id,
                    step,
                    std::mem::take(&mut pending_text),
                );
            }
            progress_publisher.publish(ModelProgress::Reset {
                task_id: task_id.clone(),
                step,
            });
        }
    };
    let response =
        provider.next_with_progress_classified(&task, &tools, cancellation, &mut emit_progress);
    drop(emit_progress);
    if !pending_text.is_empty() {
        publish_text_progress(&progress_publisher, &task_id, step, pending_text);
    }
    Ok(Some((task, response)))
}

fn publish_text_progress(publisher: &ProgressPublisher, task_id: &str, step: u32, text: String) {
    publisher.publish(ModelProgress::TextDelta {
        task_id: task_id.into(),
        step,
        text,
    });
}

fn run_tool_step(
    kernel: &Arc<Mutex<Kernel>>,
    execution: PreparedToolExecution,
    cancellation: &CancellationToken,
    stopping: &AtomicBool,
) -> fliuno_core::Result<Option<ToolResponse>> {
    if cancellation.is_cancelled() || stopping.load(Ordering::Acquire) {
        return Ok(None);
    }
    let started = kernel
        .lock()
        .map_err(|_| Error::Conflict("kernel lock is poisoned".into()))?
        .begin_tool_execution(&execution)?;
    if !started || cancellation.is_cancelled() || stopping.load(Ordering::Acquire) {
        return Ok(None);
    }
    let response = execution.execute(cancellation);
    Ok(Some((execution, response)))
}

fn schedule_followup_model(
    sender: &Sender<ScheduledTask>,
    active: &ActiveTasks,
    kernel: &mut Kernel,
    task: &Task,
) -> fliuno_core::Result<()> {
    let mut active = active
        .lock()
        .map_err(|_| Error::Conflict("task scheduler lock is poisoned".into()))?;
    if active.contains_key(&task.id) {
        return Ok(());
    }
    let cancellation = CancellationToken::new();
    active.insert(task.id.clone(), cancellation.clone());
    let job = ScheduledTask {
        id: task.id.clone(),
        cancellation,
        work: ScheduledWork::Model,
    };
    if let Err(error) = sender.try_send(job) {
        active.remove(&task.id);
        let message = match error {
            TrySendError::Full(_) => "task scheduler queue is full",
            TrySendError::Disconnected(_) => "task scheduler is unavailable",
        };
        kernel.fail_task(&task.id, message.into())?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use fliuno_core::{DemoProvider, ModelDecision, ModelProvider, ToolSpec};
    use fliuno_protocol::TaskStatus;
    use std::{
        fs,
        sync::mpsc,
        time::{Duration, Instant, SystemTime, UNIX_EPOCH},
    };

    struct BlockingModel {
        entered: mpsc::Sender<()>,
        release: Mutex<mpsc::Receiver<()>>,
    }

    impl ModelProvider for BlockingModel {
        fn next(
            &self,
            _task: &Task,
            _tools: &[ToolSpec],
            _cancellation: &CancellationToken,
        ) -> Result<ModelTurn, String> {
            self.entered.send(()).map_err(|_| "test receiver closed")?;
            self.release
                .lock()
                .map_err(|_| "test receiver lock poisoned")?
                .recv_timeout(Duration::from_secs(5))
                .map_err(|_| "test fixture not released")?;
            Ok(ModelDecision::Complete("shutdown result".into()).into())
        }
    }

    struct ConcurrentModel {
        entered: mpsc::Sender<()>,
        release: Arc<(Mutex<bool>, std::sync::Condvar)>,
    }

    impl ModelProvider for ConcurrentModel {
        fn next(
            &self,
            _task: &Task,
            _tools: &[ToolSpec],
            _cancellation: &CancellationToken,
        ) -> Result<ModelTurn, String> {
            self.entered.send(()).map_err(|_| "test receiver closed")?;
            let (lock, changed) = &*self.release;
            let released = lock.lock().map_err(|_| "test lock poisoned")?;
            let (released, _) = changed
                .wait_timeout_while(released, Duration::from_secs(5), |released| !*released)
                .map_err(|_| "test lock poisoned")?;
            if !*released {
                return Err("test fixture not released".into());
            }
            Ok(ModelDecision::Complete("concurrent result".into()).into())
        }
    }

    #[test]
    fn worker_pool_runs_to_its_concurrency_budget() {
        let (entered_tx, entered_rx) = mpsc::channel();
        let release = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
        let mut kernel = Kernel::in_memory(Box::new(ConcurrentModel {
            entered: entered_tx,
            release: Arc::clone(&release),
        }))
        .unwrap();
        let workspace = kernel
            .open_workspace(std::env::current_dir().unwrap().to_str().unwrap())
            .unwrap();
        let session = kernel.create_session(&workspace.id).unwrap();
        let tasks = (0..=WORKER_COUNT)
            .map(|index| {
                kernel
                    .create_task_with_key(&session.id, &format!("task-{index}"), None)
                    .unwrap()
                    .0
            })
            .collect::<Vec<_>>();
        let kernel = Arc::new(Mutex::new(kernel));
        let scheduler = TaskScheduler::new(Arc::clone(&kernel));
        for task in &tasks {
            scheduler.schedule(&task.id).unwrap();
        }

        for _ in 0..WORKER_COUNT {
            entered_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        }
        assert!(entered_rx.recv_timeout(Duration::from_millis(50)).is_err());

        let (lock, changed) = &*release;
        *lock.lock().unwrap() = true;
        changed.notify_all();
        let deadline = Instant::now() + Duration::from_secs(2);
        while !scheduler.is_idle() {
            assert!(
                Instant::now() < deadline,
                "scheduler did not finish all tasks"
            );
            thread::sleep(Duration::from_millis(5));
        }
        for task in tasks {
            assert_eq!(
                kernel.lock().unwrap().get_task(&task.id).unwrap().status,
                TaskStatus::Completed
            );
        }
    }

    #[test]
    fn shutdown_joins_worker_interrupts_pending_tasks_and_releases_database() {
        let root = std::env::temp_dir().join(format!(
            "fliuno-shutdown-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&root).unwrap();
        let db = root.join("kernel.db");
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let mut kernel = Kernel::open(
            &db,
            Box::new(BlockingModel {
                entered: entered_tx,
                release: Mutex::new(release_rx),
            }),
        )
        .unwrap();
        let workspace = kernel.open_workspace(root.to_str().unwrap()).unwrap();
        let session = kernel.create_session(&workspace.id).unwrap();
        let (first, _) = kernel
            .create_task_with_key(&session.id, "active", None)
            .unwrap();
        let (queued, _) = kernel
            .create_task_with_key(&session.id, "queued", None)
            .unwrap();
        let kernel = Arc::new(Mutex::new(kernel));
        let scheduler = TaskScheduler::with_worker_count(Arc::clone(&kernel), 1);
        scheduler.schedule(&first.id).unwrap();
        entered_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        scheduler.schedule(&queued.id).unwrap();
        let stopping = Arc::clone(&scheduler.stopping);
        let shutdown = thread::spawn(move || drop(scheduler));
        let deadline = Instant::now() + Duration::from_secs(2);
        while !stopping.load(Ordering::Acquire) {
            assert!(
                Instant::now() < deadline,
                "scheduler did not start shutdown"
            );
            thread::sleep(Duration::from_millis(5));
        }
        release_tx.send(()).unwrap();
        shutdown.join().unwrap();
        assert!(
            entered_rx.try_recv().is_err(),
            "queued model request executed during shutdown"
        );
        drop(kernel);
        let kernel = Kernel::open(&db, Box::new(DemoProvider)).unwrap();
        let first = kernel.get_task(&first.id).unwrap();
        let queued = kernel.get_task(&queued.id).unwrap();
        assert_eq!(first.status, TaskStatus::Interrupted);
        assert_eq!(first.model_steps, 1);
        assert!(first.model_outcome_unknown);
        assert!(first.output.is_none());
        assert_eq!(queued.status, TaskStatus::Interrupted);
        assert_eq!(queued.model_steps, 0);
        assert!(!queued.model_outcome_unknown);
        drop(kernel);
        fs::remove_dir_all(root).unwrap();
    }
}
