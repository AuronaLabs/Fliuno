//! Bounded workers run independent task steps without holding database locks.
use crossbeam_channel::{Receiver, Sender, TrySendError, bounded};
use fliuno_core::diagnostics::{Operation, Outcome, Span};
use fliuno_core::{
    AgentObservation, AgentObserver, AgentOutcome, CancellationToken, CompletedRemoteTaskCheck,
    ContextCompaction, Error, Kernel, ModelError, ModelStreamEvent, ModelTurn, PreparedModelStep,
    PreparedRemoteTaskCheck, PreparedToolExecution, ProgressPublisher, ToolError, ToolOutput,
};
use fliuno_protocol::{ModelProgress, Task};
use std::{
    collections::{HashMap, HashSet},
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

mod deadlines;

pub(super) const QUEUE_CAPACITY: usize = 32;
pub(super) const WORKER_COUNT: usize = 4;
pub(super) const REMOTE_CHECK_QUEUE_CAPACITY: usize = 8;
const MAX_MODEL_PROGRESS_DELTA_BYTES: usize = 1024;
type Cancellation = CancellationToken;
type ActiveTasks = Arc<Mutex<HashMap<String, Reservation>>>;

struct Reservation {
    cancellation: Cancellation,
    remote_check: bool,
}
enum ModelResponse {
    Generation(Task, Result<ModelTurn, ModelError>),
    Compaction(Task, Result<ContextCompaction, ModelError>),
}
type ToolResponse = (PreparedToolExecution, Result<ToolOutput, ToolError>);
type AgentResponse = (String, String, AgentOutcome);

enum ScheduledWork {
    Model,
    Agent,
    CheckRemote(Box<PreparedRemoteTaskCheck>),
    Tool(Box<PreparedToolExecution>),
    Stop,
}

enum ScheduledResponse {
    Model(Box<ModelResponse>),
    Tool(Box<ToolResponse>),
    Agent(Box<AgentResponse>),
    CheckRemote(Box<CompletedRemoteTaskCheck>),
}

struct ScheduledTask {
    id: String,
    cancellation: Cancellation,
    work: ScheduledWork,
}

pub(super) struct TaskScheduler {
    sender: Option<Sender<ScheduledTask>>,
    remote: Option<(Sender<ScheduledTask>, JoinHandle<()>)>,
    deadlines: Option<(Sender<()>, JoinHandle<()>)>,
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
        let spawn_worker =
            |name, queue: &Receiver<ScheduledTask>, submit: &Sender<ScheduledTask>| {
                let worker_kernel = Arc::clone(&kernel);
                let worker_receiver = queue.clone();
                let worker_sender = submit.clone();
                let worker_active = Arc::clone(&active);
                let worker_stopping = Arc::clone(&stopping);
                thread::Builder::new()
                    .name(name)
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
            };
        let workers = (0..worker_count)
            .map(|index| spawn_worker(format!("fliuno-task-scheduler-{index}"), &receiver, &sender))
            .collect();
        // Long-poll reads must not hold a worker needed for a new model, tool,
        // or agent step. The reservation and event commit rules are shared.
        let remote = kernel
            .lock()
            .unwrap()
            .remote_task_check_supported()
            .then(|| {
                let (submit, queue) = bounded(REMOTE_CHECK_QUEUE_CAPACITY);
                let worker = spawn_worker("fliuno-remote-checks".into(), &queue, &submit);
                (submit, worker)
            });
        let deadlines = deadlines::start(Arc::clone(&kernel), Arc::clone(&active));
        Self {
            sender: Some(sender),
            remote,
            deadlines: Some(deadlines),
            active,
            stopping,
            kernel,
            workers,
        }
    }

    pub(super) fn remote_check_workers(&self) -> usize {
        usize::from(self.remote.is_some())
    }

    pub(super) fn schedule(&self, task_id: &str) -> Result<(), &'static str> {
        self.schedule_work(task_id, ScheduledWork::Model)
            .map(|_| ())
    }

    pub(super) fn schedule_agent(&self, task_id: &str) -> Result<(), &'static str> {
        self.schedule_work(task_id, ScheduledWork::Agent)
            .map(|_| ())
    }

    pub(super) fn schedule_tool(
        &self,
        execution: Box<PreparedToolExecution>,
    ) -> Result<(), &'static str> {
        let task_id = execution.task_id().to_owned();
        self.schedule_work(&task_id, ScheduledWork::Tool(execution))
            .map(|_| ())
    }

    pub(super) fn schedule_remote_check(
        &self,
        check: PreparedRemoteTaskCheck,
    ) -> Result<(), &'static str> {
        let task_id = check.task_id().to_owned();
        if self.schedule_work(&task_id, ScheduledWork::CheckRemote(Box::new(check)))? {
            Ok(())
        } else {
            Err("task already has scheduled work")
        }
    }

    fn schedule_work(&self, task_id: &str, work: ScheduledWork) -> Result<bool, &'static str> {
        let remote_check = matches!(&work, ScheduledWork::CheckRemote(_));
        let sender = if remote_check {
            self.remote.as_ref().map(|(submit, _)| submit)
        } else {
            self.sender.as_ref()
        }
        .ok_or("task scheduler is unavailable")?;
        let mut active = self
            .active
            .lock()
            .map_err(|_| "task scheduler lock is poisoned")?;
        if active.contains_key(task_id) {
            return Ok(false);
        }
        let cancellation = CancellationToken::new();
        active.insert(
            task_id.into(),
            Reservation {
                cancellation: cancellation.clone(),
                remote_check,
            },
        );
        let job = ScheduledTask {
            id: task_id.into(),
            cancellation,
            work,
        };
        if let Err(error) = sender.try_send(job) {
            active.remove(task_id);
            return Err(match error {
                TrySendError::Full(_) if remote_check => "remote task check queue is full",
                TrySendError::Full(_) => "task scheduler queue is full",
                TrySendError::Disconnected(_) => "task scheduler is unavailable",
            });
        }
        Ok(true)
    }

    pub(super) fn cancel(&self, task_id: &str) -> Result<(), &'static str> {
        self.cancel_all([task_id])
    }

    pub(super) fn cancel_all<'a>(
        &self,
        tasks: impl IntoIterator<Item = &'a str>,
    ) -> Result<(), &'static str> {
        signal_cancellations(&self.active, tasks)
    }

    pub(super) fn notify_run_deadline(&self) {
        if let Some((sender, _)) = &self.deadlines {
            let _ = sender.try_send(());
        }
    }

    #[cfg(test)]
    pub(super) fn is_idle(&self) -> bool {
        self.active.lock().unwrap().is_empty()
    }
}

impl Drop for TaskScheduler {
    fn drop(&mut self) {
        self.stopping.store(true, Ordering::Release);
        if let Some((sender, worker)) = self.deadlines.take() {
            drop(sender);
            if worker.join().is_err() {
                eprintln!("fliuno: run deadline worker panicked");
            }
        }
        if let Ok(active) = self.active.lock() {
            for reservation in active.values() {
                reservation.cancellation.cancel();
            }
        }
        if let Some(sender) = self.sender.take() {
            wake_workers(&sender, self.workers.len());
        }
        let remote = self.remote.take().map(|(sender, worker)| {
            wake_workers(&sender, 1);
            worker
        });
        // Joining prevents abandoned workers retaining the DB lock or running
        // queued requests; providers must observe cancellation to exit promptly.
        for worker in self.workers.drain(..).chain(remote) {
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

fn signal_cancellations<'a>(
    active: &ActiveTasks,
    tasks: impl IntoIterator<Item = &'a str>,
) -> Result<(), &'static str> {
    let reservations = active
        .lock()
        .map_err(|_| "task scheduler lock is poisoned")?;
    for task_id in tasks {
        if let Some(reservation) = reservations.get(task_id).filter(|item| !item.remote_check) {
            reservation.cancellation.cancel();
        }
    }
    Ok(())
}

fn wake_workers(sender: &Sender<ScheduledTask>, count: usize) {
    for _ in 0..count {
        if sender
            .try_send(ScheduledTask {
                id: String::new(),
                cancellation: CancellationToken::new(),
                work: ScheduledWork::Stop,
            })
            .is_err()
        {
            // A full queue already has work to wake the stopped workers.
            break;
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
            ScheduledWork::Model => {
                run_model_or_recover_tool(&kernel, &id, &cancellation, &stopping)
            }
            ScheduledWork::Tool(execution) => {
                run_tool_step(&kernel, *execution, &cancellation, &stopping)
                    .map(|response| response.map(Box::new).map(ScheduledResponse::Tool))
            }
            ScheduledWork::Agent => run_agent_step(&kernel, &id, &cancellation, &stopping)
                .map(|response| response.map(Box::new).map(ScheduledResponse::Agent)),
            ScheduledWork::CheckRemote(check) => Ok(Some(ScheduledResponse::CheckRemote(
                Box::new(check.execute(&cancellation)),
            ))),
            ScheduledWork::Stop => unreachable!("stop jobs exit before dispatch"),
        };
        let Ok(mut kernel) = kernel.lock() else {
            eprintln!("fliuno: kernel lock is poisoned");
            break;
        };
        // Resolve an elapsed deadline before accepting a late tool or model result.
        let deadline = kernel.expire_run_for_task(&id).map(|expired| {
            if let Some(ids) = expired
                && let Err(error) = signal_cancellations(&active, ids.iter().map(String::as_str))
            {
                eprintln!("fliuno: signalling expired run: {error}");
            }
        });
        // Release the reservation before exposing another approval or step.
        if let Ok(mut active) = active.lock() {
            active.remove(&id);
        }
        if stopping.load(Ordering::Acquire) {
            break;
        }
        if let Err(error) = deadline {
            eprintln!("fliuno: checking run deadline: {error}");
            continue;
        }
        let result = match response {
            Ok(Some(ScheduledResponse::CheckRemote(check))) => {
                kernel.record_remote_task_check(*check)
            }
            Ok(Some(ScheduledResponse::Agent(response))) => {
                let (task_id, run_id, outcome) = *response;
                kernel
                    .finish_agent_run(&task_id, &run_id, outcome)
                    .map(|_| ())
            }
            Ok(Some(ScheduledResponse::Model(response))) => match *response {
                ModelResponse::Generation(task, response) => {
                    match kernel.apply_model_response(task, response) {
                        Ok(task) if task.status == fliuno_protocol::TaskStatus::Running => {
                            schedule_followup_model(&sender, &active, &mut kernel, &task)
                        }
                        Ok(_) => Ok(()),
                        Err(error) => Err(error),
                    }
                }
                ModelResponse::Compaction(task, response) => {
                    match kernel.apply_context_compaction(task, response) {
                        Ok(task) if task.status == fliuno_protocol::TaskStatus::Running => {
                            schedule_followup_model(&sender, &active, &mut kernel, &task)
                        }
                        Ok(_) => Ok(()),
                        Err(error) => Err(error),
                    }
                }
            },
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
        match result {
            Err(error) => eprintln!("fliuno: task scheduler: {error}"),
            Ok(()) => match kernel.advance_runs_for_task(&id) {
                Ok(tasks) => {
                    if let Err(error) = schedule_run_followups(&sender, &active, &mut kernel, tasks)
                    {
                        eprintln!("fliuno: task run scheduler: {error}");
                    }
                }
                Err(error) => eprintln!("fliuno: task run update: {error}"),
            },
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
    let (prepared, scope) = {
        let mut kernel = kernel
            .lock()
            .map_err(|_| Error::Conflict("kernel lock is poisoned".into()))?;
        (
            kernel.prepare_agent_run(task_id, cancellation)?,
            kernel.diagnostic_scope(task_id),
        )
    };
    let Some(prepared) = prepared else {
        return Ok(None);
    };
    let observer = PersistentAgentObserver {
        kernel: Arc::clone(kernel),
        task_id: task_id.into(),
        run_id: prepared.request.run_id.clone(),
    };
    let mut span = scope.and_then(|scope| scope.begin(Operation::Agent));
    let outcome = if cancellation.is_cancelled() || stopping.load(Ordering::Acquire) {
        AgentOutcome::unknown()
    } else {
        Span::within(span.as_ref(), || prepared.execute(&observer, cancellation))
    };
    if let Some(span) = &mut span {
        span.finish(if cancellation.is_cancelled() {
            Outcome::Cancelled
        } else if outcome.outcome_unknown {
            Outcome::Interrupted
        } else {
            Outcome::Completed
        });
    }
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
    let (prepared, provider, progress_publisher, scope) = {
        let mut kernel = kernel
            .lock()
            .map_err(|_| Error::Conflict("kernel lock is poisoned".into()))?;
        (
            kernel.prepare_model_step(task_id)?,
            kernel.model_provider_handle(),
            kernel.model_progress_publisher(),
            kernel.diagnostic_scope(task_id),
        )
    };
    let PreparedModelStep::Ready { task, tools } = prepared else {
        return Ok(None);
    };
    if cancellation.is_cancelled() || stopping.load(Ordering::Acquire) {
        return Ok(None);
    }
    if fliuno_core::context_compaction_pending(&task) {
        let mut span = scope.and_then(|scope| scope.begin(Operation::ContextCompaction));
        let result = catch_unwind(AssertUnwindSafe(|| {
            fliuno_core::execute_context_compaction(provider.as_ref(), &task, cancellation)
        }))
        .unwrap_or_else(|_| {
            Err(ModelError::OutcomeUnknown(
                "context summarizer panicked".into(),
            ))
        });
        if let Some(span) = &mut span {
            span.model_error(task.model_steps, result.as_ref().err());
        }
        return Ok(Some(ModelResponse::Compaction(task, result)));
    }
    let task_id = task.id.clone();
    let step = task.model_steps;
    let mut pending_text = String::new();
    let mut last_publish = Instant::now();
    let mut has_published_text = false;
    let mut emit_progress = |update| match update {
        ModelStreamEvent::TextDelta(text) => append_text_progress(
            &progress_publisher,
            &task_id,
            step,
            &mut pending_text,
            &mut last_publish,
            &mut has_published_text,
            &text,
        ),
        ModelStreamEvent::Reset => {
            flush_text_progress(
                &progress_publisher,
                &task_id,
                step,
                &mut pending_text,
                &mut last_publish,
                &mut has_published_text,
            );
            progress_publisher.publish(ModelProgress::Reset {
                task_id: task_id.clone(),
                step,
            });
        }
    };
    let mut span = scope.and_then(|scope| scope.begin(Operation::Model));
    let response = catch_unwind(AssertUnwindSafe(|| {
        provider
            .next_with_progress_classified(&task, &tools, cancellation, &mut emit_progress)
            .map_err(|error| error.with_input_attribution(&task, &tools))
    }));
    drop(emit_progress);
    let response = match response {
        Ok(response) => {
            if !pending_text.is_empty() {
                publish_text_progress(&progress_publisher, &task_id, step, pending_text);
            }
            response
        }
        Err(_) => {
            if has_published_text {
                progress_publisher.publish(ModelProgress::Reset { task_id, step });
            }
            Err(ModelError::OutcomeUnknown("model provider panicked".into()))
        }
    };
    if let Some(span) = &mut span {
        span.model(step, &response);
    }
    Ok(Some(ModelResponse::Generation(task, response)))
}

fn append_text_progress(
    publisher: &ProgressPublisher,
    task_id: &str,
    step: u32,
    pending: &mut String,
    last_publish: &mut Instant,
    has_published_text: &mut bool,
    text: &str,
) {
    let mut remaining = text;
    while !remaining.is_empty() {
        let available = MAX_MODEL_PROGRESS_DELTA_BYTES.saturating_sub(pending.len());
        if available == 0 {
            flush_text_progress(
                publisher,
                task_id,
                step,
                pending,
                last_publish,
                has_published_text,
            );
            continue;
        }
        let mut length = remaining.len().min(available);
        while !remaining.is_char_boundary(length) {
            length -= 1;
        }
        if length == 0 {
            flush_text_progress(
                publisher,
                task_id,
                step,
                pending,
                last_publish,
                has_published_text,
            );
            continue;
        }
        pending.push_str(&remaining[..length]);
        remaining = &remaining[length..];
        if !*has_published_text
            || pending.len() >= MAX_MODEL_PROGRESS_DELTA_BYTES
            || last_publish.elapsed() >= Duration::from_millis(30)
        {
            flush_text_progress(
                publisher,
                task_id,
                step,
                pending,
                last_publish,
                has_published_text,
            );
        }
    }
}

fn flush_text_progress(
    publisher: &ProgressPublisher,
    task_id: &str,
    step: u32,
    pending: &mut String,
    last_publish: &mut Instant,
    has_published_text: &mut bool,
) {
    if pending.is_empty() {
        return;
    }
    publish_text_progress(publisher, task_id, step, std::mem::take(pending));
    *last_publish = Instant::now();
    *has_published_text = true;
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
    let ((started, ticket), scope) = {
        let mut kernel = kernel
            .lock()
            .map_err(|_| Error::Conflict("kernel lock is poisoned".into()))?;
        (
            kernel.begin_scheduled_tool(&execution)?,
            kernel.diagnostic_scope(execution.task_id()),
        )
    };
    if !started || cancellation.is_cancelled() || stopping.load(Ordering::Acquire) {
        return Ok(None);
    }
    let mut span = scope.and_then(|scope| scope.begin(Operation::Tool));
    let response = Span::within(span.as_ref(), || match ticket {
        Some(ticket) => execution.observe_supervisor(ticket, true, stopping),
        None => Some(execution.execute(cancellation)),
    });
    if let (Some(span), Some(response)) = (&mut span, &response) {
        span.tool(response, cancellation.is_cancelled());
    }
    Ok(response.map(|response| (execution, response)))
}

fn run_model_or_recover_tool(
    kernel: &Arc<Mutex<Kernel>>,
    id: &str,
    cancellation: &CancellationToken,
    stopping: &AtomicBool,
) -> fliuno_core::Result<Option<ScheduledResponse>> {
    let recovery = kernel
        .lock()
        .map_err(|_| Error::Conflict("kernel lock is poisoned".into()))?
        .prepare_supervisor_observation(id)?;
    if let Some((execution, ticket)) = recovery {
        return Ok(execution
            .observe_supervisor(ticket, false, stopping)
            .map(|response| ScheduledResponse::Tool(Box::new((execution, response)))));
    }
    run_model_step(kernel, id, cancellation, stopping)
        .map(|response| response.map(Box::new).map(ScheduledResponse::Model))
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
    active.insert(
        task.id.clone(),
        Reservation {
            cancellation: cancellation.clone(),
            remote_check: false,
        },
    );
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

fn schedule_run_followups(
    sender: &Sender<ScheduledTask>,
    active: &ActiveTasks,
    kernel: &mut Kernel,
    tasks: Vec<Task>,
) -> fliuno_core::Result<()> {
    let mut pending = tasks;
    let mut visited = HashSet::new();
    while let Some(task) = pending.pop() {
        let task_id = task.id.clone();
        if !visited.insert(task_id.clone()) {
            continue;
        }
        schedule_followup_model(sender, active, kernel, &task)?;
        pending.extend(kernel.advance_runs_for_task(&task_id)?);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use fliuno_core::{DemoProvider, Kernel, ModelDecision, ModelProvider, ToolSpec};
    use fliuno_protocol::TaskStatus;
    use std::{
        fs,
        sync::mpsc,
        time::{Duration, Instant, SystemTime, UNIX_EPOCH},
    };

    #[test]
    fn large_provider_delta_is_split_at_utf8_boundaries_before_fanout() {
        let kernel = Kernel::in_memory(Box::new(DemoProvider)).unwrap();
        let receiver = kernel.subscribe_model_progress().unwrap();
        let publisher = kernel.model_progress_publisher();
        let expected = format!(
            "{}{}{}",
            "a".repeat(1021),
            "你好😀".repeat(900),
            "z".repeat(2048)
        );
        let mut pending = String::new();
        let mut last_publish = Instant::now();
        let mut has_published_text = false;

        append_text_progress(
            &publisher,
            "task_large_delta",
            1,
            &mut pending,
            &mut last_publish,
            &mut has_published_text,
            &expected,
        );
        flush_text_progress(
            &publisher,
            "task_large_delta",
            1,
            &mut pending,
            &mut last_publish,
            &mut has_published_text,
        );

        let chunks: Vec<String> = receiver
            .try_iter()
            .map(|progress| match progress {
                ModelProgress::TextDelta {
                    task_id,
                    step,
                    text,
                } => {
                    assert_eq!(task_id, "task_large_delta");
                    assert_eq!(step, 1);
                    assert!(text.len() <= MAX_MODEL_PROGRESS_DELTA_BYTES);
                    text
                }
                ModelProgress::Reset { .. } => panic!("unexpected reset"),
            })
            .collect();
        assert!(chunks.len() > 1);
        assert_eq!(chunks.concat(), expected);
    }

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

    struct FaultyModel;

    impl ModelProvider for FaultyModel {
        fn next(
            &self,
            task: &Task,
            _tools: &[ToolSpec],
            _cancellation: &CancellationToken,
        ) -> Result<ModelTurn, String> {
            match &task.messages[0] {
                fliuno_protocol::Message::User { text } if text == "invoke tool" => {
                    Ok(ModelDecision::Call(fliuno_protocol::ToolCall {
                        id: "fixture-call".into(),
                        name: "faulty_tool".into(),
                        arguments: serde_json::json!({}),
                    })
                    .into())
                }
                _ => Ok(ModelDecision::Complete("worker still available".into()).into()),
            }
        }

        fn next_with_progress(
            &self,
            task: &Task,
            tools: &[ToolSpec],
            cancellation: &CancellationToken,
            progress: &mut dyn FnMut(ModelStreamEvent),
        ) -> Result<ModelTurn, String> {
            if matches!(&task.messages[0], fliuno_protocol::Message::User { text } if text == "crash model")
            {
                progress(ModelStreamEvent::TextDelta("partial".into()));
                panic!("fixture provider diagnostic");
            }
            self.next(task, tools, cancellation)
        }
    }

    struct FaultyTool;

    impl fliuno_core::Tool for FaultyTool {
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: "faulty_tool".into(),
                description: "A tool that fails after dispatch".into(),
                effect: fliuno_core::ToolEffect::External,
                input_schema: serde_json::json!({"type":"object"}),
            }
        }

        fn validate(&self, _arguments: &serde_json::Value) -> Result<(), String> {
            Ok(())
        }

        fn execute(
            &self,
            _context: &fliuno_core::ToolContext<'_>,
            _call: &fliuno_protocol::ToolCall,
            _cancellation: &CancellationToken,
        ) -> Result<ToolOutput, ToolError> {
            panic!("fixture tool diagnostic");
        }
    }

    struct FaultyAgent {
        check_gate: Option<(Sender<()>, Receiver<()>)>,
    }

    impl fliuno_core::AgentBackend for FaultyAgent {
        fn spec(&self) -> fliuno_protocol::AgentSpec {
            fliuno_protocol::AgentSpec {
                id: "faulty-agent".into(),
                protocol: "fixture".into(),
                isolation: "fixture".into(),
                workspace_access: false,
            }
        }

        fn supports_remote_task_check(&self) -> bool {
            true
        }

        fn check_remote_task(
            &self,
            remote: &fliuno_protocol::AgentRemoteTask,
            _cancellation: &CancellationToken,
        ) -> Option<fliuno_protocol::AgentRemoteTask> {
            if let Some((entered, release)) = &self.check_gate {
                entered.send(()).unwrap();
                release.recv_timeout(Duration::from_secs(8)).ok()?;
                return Some(remote.clone());
            }
            panic!("fixture check diagnostic");
        }

        fn execute(
            &self,
            _request: &fliuno_core::AgentRequest,
            observer: &dyn AgentObserver,
            _commands: tokio::sync::mpsc::Receiver<fliuno_core::AgentCommand>,
            _cancellation: &CancellationToken,
        ) -> AgentOutcome {
            observer
                .observe(AgentObservation::RemoteTask(
                    fliuno_protocol::AgentRemoteTask {
                        id: "fixture-remote".into(),
                        context_id: None,
                        state: fliuno_protocol::AgentTaskState::Working,
                    },
                ))
                .unwrap();
            panic!("fixture agent diagnostic");
        }
    }

    #[test]
    fn adapter_panics_preserve_uncertain_outcomes_and_keep_the_only_worker_available() {
        let mut kernel = Kernel::in_memory(Box::new(FaultyModel)).unwrap();
        kernel.register_tool(Box::new(FaultyTool)).unwrap();
        kernel
            .register_agent(Box::new(FaultyAgent { check_gate: None }))
            .unwrap();
        let root = std::env::current_dir().unwrap();
        let workspace = kernel.open_workspace(root.to_str().unwrap()).unwrap();
        let session = kernel.create_session(&workspace.id).unwrap();
        let model = kernel
            .create_task_with_key(&session.id, "crash model", None)
            .unwrap()
            .0;
        let tool = kernel
            .create_task_with_key(&session.id, "invoke tool", None)
            .unwrap()
            .0;
        let agent = kernel
            .create_task_with_backend(
                &session.id,
                "crash agent",
                None,
                fliuno_protocol::TaskBackend::ExternalAgent {
                    agent_id: "faulty-agent".into(),
                },
            )
            .unwrap()
            .0;
        let healthy = kernel
            .create_task_with_key(&session.id, "healthy", None)
            .unwrap()
            .0;
        let progress = kernel.subscribe_model_progress().unwrap();
        let kernel = Arc::new(Mutex::new(kernel));
        let scheduler = TaskScheduler::with_worker_count(Arc::clone(&kernel), 1);
        let wait = |id: &str, expected| {
            let deadline = Instant::now() + Duration::from_secs(3);
            loop {
                let task = kernel.lock().unwrap().get_task(id).unwrap();
                if task.status == expected {
                    return task;
                }
                assert!(Instant::now() < deadline, "{id} remained {:?}", task.status);
                thread::sleep(Duration::from_millis(5));
            }
        };
        scheduler.schedule(&model.id).unwrap();
        let model = wait(&model.id, TaskStatus::Interrupted);
        assert!(model.model_outcome_unknown);
        assert_eq!(
            model.output.as_deref(),
            Some("model outcome unknown: model provider panicked")
        );
        assert!(matches!(
            progress.try_recv().unwrap(),
            ModelProgress::TextDelta { .. }
        ));
        assert_eq!(
            progress.try_recv().unwrap(),
            ModelProgress::Reset {
                task_id: model.id.clone(),
                step: 1
            }
        );
        assert!(
            kernel
                .lock()
                .unwrap()
                .prepare_resume_task(&model.id, false)
                .is_err()
        );

        scheduler.schedule(&tool.id).unwrap();
        let pending = wait(&tool.id, TaskStatus::WaitingApproval)
            .pending_approval
            .unwrap();
        let fliuno_core::ApprovalOutcome::Prepared { execution, .. } = kernel
            .lock()
            .unwrap()
            .prepare_approval(
                &tool.id,
                &pending.id,
                fliuno_protocol::ApprovalDecision::AllowOnce,
            )
            .unwrap()
        else {
            panic!("approval did not prepare the operation")
        };
        scheduler.schedule_tool(execution).unwrap();
        let tool = wait(&tool.id, TaskStatus::Interrupted);
        assert_eq!(tool.in_flight_tool.unwrap().id, "fixture-call");
        assert!(
            kernel
                .lock()
                .unwrap()
                .prepare_resume_task(&tool.id, true)
                .is_err()
        );

        scheduler.schedule_agent(&agent.id).unwrap();
        let run = wait(&agent.id, TaskStatus::Interrupted).agent_run.unwrap();
        assert!(run.finished && run.outcome_unknown);
        assert_eq!(run.remote_task.unwrap().id, "fixture-remote");
        let check = kernel
            .lock()
            .unwrap()
            .prepare_remote_task_check(&agent.id)
            .unwrap();
        let check_id = check.check_id().to_owned();
        scheduler.schedule_remote_check(check).unwrap();
        scheduler.schedule(&healthy.id).unwrap();
        assert_eq!(
            wait(&healthy.id, TaskStatus::Completed).output.as_deref(),
            Some("worker still available")
        );
        let deadline = Instant::now() + Duration::from_secs(3);
        while !scheduler.is_idle() {
            assert!(Instant::now() < deadline, "remote check did not finish");
            thread::sleep(Duration::from_millis(5));
        }
        let events = kernel
            .lock()
            .unwrap()
            .list_events(&agent.id, 0, 100)
            .unwrap()
            .events;
        assert!(events.iter().any(|event| matches!(
            &event.data,
            fliuno_protocol::EventKind::AgentRemoteTaskChecked {
                check_id: observed,
                outcome: fliuno_protocol::RemoteTaskCheckOutcome::Unavailable,
                ..
            } if observed == &check_id
        )));
    }

    #[test]
    fn an_in_flight_remote_check_does_not_occupy_the_task_worker() {
        let (entered, checking) = bounded(1);
        let (release, gate) = bounded(1);
        let mut kernel = Kernel::in_memory(Box::new(DemoProvider)).unwrap();
        kernel
            .register_agent(Box::new(FaultyAgent {
                check_gate: Some((entered, gate)),
            }))
            .unwrap();
        let root = std::env::current_dir().unwrap();
        let workspace = kernel.open_workspace(root.to_str().unwrap()).unwrap();
        let session = kernel.create_session(&workspace.id).unwrap();
        let agent = kernel
            .create_task_with_backend(
                &session.id,
                "record an uncertain remote task",
                None,
                fliuno_protocol::TaskBackend::ExternalAgent {
                    agent_id: "faulty-agent".into(),
                },
            )
            .unwrap()
            .0;
        let model = kernel
            .create_task_with_key(&session.id, "a separate task", None)
            .unwrap()
            .0;
        let kernel = Arc::new(Mutex::new(kernel));
        let scheduler = TaskScheduler::with_worker_count(Arc::clone(&kernel), 1);
        let wait = |id: &str, expected| {
            let deadline = Instant::now() + Duration::from_secs(3);
            loop {
                let task = kernel.lock().unwrap().get_task(id).unwrap();
                if task.status == expected {
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "task {id} remained {:?}",
                    task.status
                );
                thread::sleep(Duration::from_millis(5));
            }
        };
        scheduler.schedule_agent(&agent.id).unwrap();
        wait(&agent.id, TaskStatus::Interrupted);
        let prepare_check = || {
            kernel
                .lock()
                .unwrap()
                .prepare_remote_task_check(&agent.id)
                .unwrap()
        };
        scheduler.schedule_remote_check(prepare_check()).unwrap();
        checking.recv_timeout(Duration::from_secs(3)).unwrap();
        assert!(scheduler.schedule_remote_check(prepare_check()).is_err());
        scheduler.schedule(&model.id).unwrap();
        wait(&model.id, TaskStatus::WaitingApproval);
        release.send(()).unwrap();
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
