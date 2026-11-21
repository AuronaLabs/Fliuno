//! Optional diagnostics carry identifiers and numeric measurements only. A
//! full queue drops a span and never blocks an execution or commits a write.
use crate::{Kernel, ModelError, ModelTurn, ToolError};
use crossbeam_channel::{Receiver, Sender};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    cell::Cell,
    sync::OnceLock,
    time::{Instant, SystemTime, UNIX_EPOCH},
};

const CAPACITY: usize = 512;
struct Sink {
    channel: Sender<SpanRecord>,
    destinations: Destinations,
}
static SINK: OnceLock<Sink> = OnceLock::new();
thread_local! {
    static CURRENT: Cell<Option<Scope>> = const { Cell::new(None) };
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Destinations {
    pub path: Option<std::path::PathBuf>,
    pub endpoint: Option<String>,
}

#[derive(Clone, Copy, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Scope {
    trace: [u8; 16],
    parent: Option<[u8; 8]>,
    run: Option<[u8; 16]>,
    task: Option<[u8; 16]>,
}

#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Operation {
    Run,
    Agent,
    Model,
    ContextCompaction,
    Tool,
    Mcp,
    Approval,
    Recovery,
    Process,
}

#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Completed,
    Rejected,
    Cancelled,
    Interrupted,
    Failed,
}

#[derive(Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Measurements {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attempt: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpu_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub peak_memory_bytes: Option<u64>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SpanRecord {
    pub operation: Operation,
    pub trace_id: String,
    pub span_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_span_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_ref: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task_ref: Option<String>,
    pub start_unix_ns: u64,
    pub duration_ns: u64,
    pub outcome: Outcome,
    pub measurements: Measurements,
}

pub fn enable(destinations: Destinations) -> Option<Receiver<SpanRecord>> {
    let (channel, receive) = crossbeam_channel::bounded(CAPACITY);
    SINK.set(Sink {
        channel,
        destinations,
    })
    .ok()?;
    Some(receive)
}

/// Prepared by the trusted host and sent only over the private worker pipe.
pub fn worker_destinations() -> Option<Destinations> {
    let mut output = SINK.get()?.destinations.clone();
    if let Some(path) = &mut output.path {
        let mut name = path.file_name()?.to_os_string();
        name.push(format!(".worker-{}.jsonl", uuid::Uuid::new_v4()));
        path.set_file_name(name);
    }
    Some(output)
}

pub fn enabled() -> bool {
    SINK.get().is_some()
}

fn digest(domain: &[u8], id: &str) -> [u8; 16] {
    let mut hash = Sha256::new();
    hash.update(b"fliuno.diagnostics.v1\0");
    hash.update(domain);
    hash.update([0]);
    hash.update(id.as_bytes());
    hash.finalize()[..16].try_into().expect("16 byte slice")
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        write!(encoded, "{byte:02x}").expect("write to String");
    }
    encoded
}

fn ns(duration: std::time::Duration) -> u64 {
    u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX)
}

impl Scope {
    pub fn for_task(task_id: &str, run_id: Option<&str>) -> Self {
        let task = digest(b"task", task_id);
        let run = run_id.map(|id| digest(b"run", id));
        Self {
            trace: run.unwrap_or(task),
            parent: None,
            run,
            task: Some(task),
        }
    }

    pub fn for_run(run_id: &str) -> Self {
        let run = digest(b"run", run_id);
        Self {
            trace: run,
            parent: None,
            run: Some(run),
            task: None,
        }
    }

    pub fn traceparent(self) -> Option<String> {
        self.parent
            .map(|parent| format!("00-{}-{}-01", hex(&self.trace), hex(&parent)))
    }

    pub fn current() -> Option<Self> {
        CURRENT.with(Cell::get)
    }

    pub fn in_scope<T>(self, operation: impl FnOnce() -> T) -> T {
        struct Reset(Option<Scope>);
        impl Drop for Reset {
            fn drop(&mut self) {
                CURRENT.with(|cell| cell.set(self.0));
            }
        }
        let _reset = Reset(CURRENT.with(|cell| cell.replace(Some(self))));
        operation()
    }

    pub fn begin(self, operation: Operation) -> Option<Span> {
        enabled().then(|| Span {
            parent: self,
            id: uuid::Uuid::new_v4().as_bytes()[..8]
                .try_into()
                .expect("8 byte slice"),
            start_unix_ns: ns(SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()),
            start: Instant::now(),
            operation,
            outcome: Outcome::Interrupted,
            measurements: Measurements::default(),
        })
    }
}

pub struct Span {
    parent: Scope,
    id: [u8; 8],
    start_unix_ns: u64,
    start: Instant,
    operation: Operation,
    outcome: Outcome,
    measurements: Measurements,
}

impl Span {
    pub fn within<T>(span: Option<&Self>, operation: impl FnOnce() -> T) -> T {
        match span {
            Some(span) => span.scope().in_scope(operation),
            None => operation(),
        }
    }

    pub fn resources(&mut self, cpu_ms: Option<u64>, peak_memory_bytes: Option<u64>) {
        self.measurements.cpu_ms = cpu_ms;
        self.measurements.peak_memory_bytes = peak_memory_bytes;
    }

    pub fn model_error(&mut self, step: u32, error: Option<&ModelError>) {
        self.measurements.attempt = Some(step);
        self.outcome = match error {
            None => Outcome::Completed,
            Some(ModelError::NotDispatched(_) | ModelError::InputBudgetExceeded(_)) => {
                Outcome::Rejected
            }
            Some(ModelError::CancelledBeforeDispatch) => Outcome::Cancelled,
            Some(ModelError::OutcomeUnknown(_)) => Outcome::Interrupted,
        };
    }

    pub fn scope(&self) -> Scope {
        Scope {
            parent: Some(self.id),
            ..self.parent
        }
    }

    pub fn finish(&mut self, outcome: Outcome) {
        self.outcome = outcome;
    }

    pub fn model(&mut self, step: u32, outcome: &Result<ModelTurn, ModelError>) {
        self.model_error(step, outcome.as_ref().err());
        if let Ok(turn) = outcome
            && let Some(usage) = &turn.usage
        {
            self.measurements.input_tokens = Some(usage.input_tokens);
            self.measurements.output_tokens = Some(usage.output_tokens);
        }
    }

    pub fn tool<T>(&mut self, outcome: &Result<T, ToolError>, cancelled: bool) {
        self.outcome = match outcome {
            _ if cancelled => Outcome::Cancelled,
            Ok(_) => Outcome::Completed,
            Err(ToolError::OutcomeUnknown(_)) => Outcome::Interrupted,
            Err(_) => Outcome::Failed,
        };
    }
}

impl Drop for Span {
    fn drop(&mut self) {
        let Some(sink) = SINK.get() else { return };
        let _ = sink.channel.try_send(SpanRecord {
            operation: self.operation,
            trace_id: hex(&self.parent.trace),
            span_id: hex(&self.id),
            parent_span_id: self.parent.parent.map(|id| hex(&id)),
            run_ref: self.parent.run.map(|id| hex(&id)),
            task_ref: self.parent.task.map(|id| hex(&id)),
            start_unix_ns: self.start_unix_ns,
            duration_ns: ns(self.start.elapsed()),
            outcome: self.outcome,
            measurements: std::mem::take(&mut self.measurements),
        });
    }
}

impl Kernel {
    pub fn diagnostic_scope(&self, task_id: &str) -> Option<Scope> {
        if !enabled() {
            return None;
        }
        let run = self.owning_run(task_id).ok().flatten();
        Some(Scope::for_task(task_id, run.as_deref()))
    }
}
