//! Connection-owned login PTY. Input and output never enter the terminal spool.
use crate::{
    config::{Isolation, Preset, PresetConfig, digest},
    protocol::dimensions,
    worker::pty::{self, IoEvent},
};
use fliuno_core::CancellationToken;
use std::{
    collections::VecDeque,
    path::PathBuf,
    sync::{Arc, Mutex},
    thread,
    time::Duration,
};
use tokio::{
    sync::mpsc,
    time::{Instant, MissedTickBehavior},
};

const OUTPUT_CAPACITY: usize = 64 * 1024;
const INPUT_LIMIT: usize = 4 * 1024;
const PAGE_LIMIT: usize = 16 * 1024;

#[derive(Clone)]
pub struct LoginPreset {
    preset: Arc<Preset>,
    cwd: PathBuf,
}

impl LoginPreset {
    pub fn resolve(
        command: PathBuf,
        args: Vec<String>,
        env_allow: Vec<String>,
        cwd: PathBuf,
        lifetime_ms: u64,
    ) -> Result<Self, String> {
        if !cfg!(any(windows, target_os = "linux"))
            || !cwd.is_absolute()
            || !cwd.is_dir()
            || !(1000..=900_000).contains(&lifetime_ms)
        {
            return Err(
                "ACP login requires a fixed directory and lifetimeMs in 1000..900000".into(),
            );
        }
        let cwd = cwd
            .canonicalize()
            .map_err(|_| "cannot resolve ACP login directory")?;
        let preset = Preset::resolve(
            PresetConfig {
                id: "acp-login".into(),
                command,
                isolation: Isolation::TrustedHost,
                args,
                env_allow,
                lifetime_ms,
                spool_bytes: OUTPUT_CAPACITY,
                resources: Default::default(),
                linux_cgroup_parent: None,
            },
            &cwd,
        )?;
        Ok(Self {
            preset: Arc::new(preset),
            cwd,
        })
    }

    pub fn start(&self, rows: u16, cols: u16) -> Result<Login, String> {
        dimensions(rows, cols)?;
        let preset = self.preset.clone();
        let cwd = self.cwd.clone();
        let state = Arc::new(Mutex::new(Output::default()));
        let output = state.clone();
        let cancellation = CancellationToken::new();
        let stop = cancellation.clone();
        let (commands, requests) = mpsc::channel(1);
        let (finished, completion) = std::sync::mpsc::channel();
        thread::Builder::new()
            .name("acp-login-pty".into())
            .spawn(move || {
                let result = run(preset, cwd, (rows, cols), requests, &output, stop);
                let mut state = output.lock().unwrap();
                state.status = result.unwrap_or(Status::Failed);
                let _ = finished.send(());
            })
            .map_err(|_| "cannot start ACP login worker")?;
        Ok(Login {
            output: state,
            cancellation,
            commands,
            completion,
            next_input: 0,
        })
    }

    pub fn identity(&self) -> &str {
        &self.preset.identity
    }
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub enum Status {
    #[default]
    Starting,
    Running,
    Closed,
    Exited(i64),
    Expired,
    Failed,
    Unknown,
}

impl Status {
    pub fn finished(self) -> bool {
        !matches!(self, Self::Starting | Self::Running)
    }
}

#[derive(Default)]
struct Output {
    status: Status,
    bytes: VecDeque<u8>,
    end: u64,
}
impl Output {
    fn append(&mut self, bytes: &[u8]) {
        let overflow = (self.bytes.len() + bytes.len()).saturating_sub(OUTPUT_CAPACITY);
        self.bytes.drain(..overflow);
        self.bytes.extend(bytes);
        self.end += bytes.len() as u64;
    }
}

pub struct Page {
    pub status: Status,
    pub bytes: Vec<u8>,
    pub next: u64,
    pub truncated: bool,
}

pub struct Login {
    output: Arc<Mutex<Output>>,
    cancellation: CancellationToken,
    commands: mpsc::Sender<String>,
    completion: std::sync::mpsc::Receiver<()>,
    next_input: u64,
}

impl Login {
    pub fn status(&self) -> Status {
        self.output.lock().unwrap().status
    }

    pub fn close(&self) {
        self.cancellation.cancel();
    }

    pub fn closing(&self) -> bool {
        self.cancellation.is_cancelled()
    }

    pub fn wait_for_close(&self, limit: Duration) {
        self.close();
        let _ = self.completion.recv_timeout(limit);
    }

    pub fn output(&self, after: u64) -> Result<Page, String> {
        let output = self.output.lock().unwrap();
        if after > output.end {
            return Err("ACP login cursor exceeds output".into());
        }
        let start = output.end - output.bytes.len() as u64;
        let from = after.max(start);
        let bytes: Vec<_> = output
            .bytes
            .iter()
            .skip((from - start) as usize)
            .take(PAGE_LIMIT)
            .copied()
            .collect();
        Ok(Page {
            status: output.status,
            next: from + bytes.len() as u64,
            bytes,
            truncated: after < start,
        })
    }

    /// A reply acknowledges admission, not OS delivery. Retrying a sequence never retypes it.
    pub fn input(&mut self, sequence: u64, data: String) -> Result<u64, String> {
        if sequence != self.next_input || data.is_empty() || data.len() > INPUT_LIMIT {
            return Err("invalid ACP login input sequence or size".into());
        }
        let next = sequence
            .checked_add(1)
            .ok_or("ACP login input sequence exhausted")?;
        if self.closing() || self.output.lock().unwrap().status != Status::Running {
            return Err("ACP login is not running".into());
        }
        self.commands
            .try_send(data)
            .map_err(|_| "ACP login input unavailable")?;
        self.next_input = next;
        Ok(next)
    }
}

impl Drop for Login {
    fn drop(&mut self) {
        self.close();
    }
}

fn run(
    preset: Arc<Preset>,
    cwd: PathBuf,
    size: (u16, u16),
    mut input: mpsc::Receiver<String>,
    output: &Mutex<Output>,
    stop: CancellationToken,
) -> Result<Status, String> {
    let deadline = Instant::now() + Duration::from_millis(preset.config.lifetime_ms);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .map_err(|_| "cannot start ACP login runtime")?;
    if digest(&preset.config.command)? != preset.executable_digest {
        return Ok(Status::Failed);
    }
    if stop.is_cancelled() {
        return Ok(Status::Closed);
    }
    if Instant::now() >= deadline {
        return Ok(Status::Expired);
    }
    let Ok((mut pty, mut events)) = pty::spawn(pty::Process {
        command: &preset.config.command,
        args: &preset.config.args,
        environment: &preset.environment,
        cwd: &cwd,
        dimensions: size,
    }) else {
        return Ok(Status::Unknown);
    };
    runtime.block_on(async {
        output.lock().unwrap().status = Status::Running;
        let mut tick = tokio::time::interval(Duration::from_millis(20));
        tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
        let mut input_pending = false;
        let state = loop {
            tokio::select! {
                _ = stop.cancelled() => break Status::Closed,
                _ = tokio::time::sleep_until(deadline) => break Status::Expired,
                _ = tick.tick() => match pty.try_wait() {
                    Ok(Some(status)) => break Status::Exited(i64::from(status.exit_code())),
                    Ok(None) => {},
                    Err(_) => break Status::Unknown,
                },
                data = input.recv(), if !input_pending => {
                    let Some(data) = data else { break Status::Unknown; };
                    if pty.write("login".into(), data).is_err() { break Status::Unknown; }
                    input_pending = true;
                },
                Some(event) = events.recv() => match event {
                    IoEvent::CursorQuery => { let _ = pty.write("startup-cursor".into(), "\x1b[1;1R".into()); },
                    IoEvent::Output(bytes) => output.lock().unwrap().append(&bytes),
                    IoEvent::Written { id, ok: true, .. } if id == "login" => input_pending = false,
                    IoEvent::Written { id, ok: false, .. } if id == "login" => break Status::Unknown,
                    IoEvent::ReadError => break Status::Unknown,
                    _ => {},
                },
            }
        };
        if pty.stop().is_err() {
            return Ok(Status::Unknown);
        }
        let cleanup = Instant::now() + Duration::from_millis(500);
        let _ = tokio::time::timeout_at(cleanup, async {
            while let Some(event) = events.recv().await {
                match event {
                    IoEvent::Output(bytes) => output.lock().unwrap().append(&bytes),
                    IoEvent::Closed | IoEvent::ReadError => break,
                    _ => {},
                }
            }
        }).await;
        loop {
            match pty.try_wait() {
                Ok(Some(_)) => return Ok(state),
                Ok(None) if Instant::now() < cleanup => tokio::time::sleep(Duration::from_millis(10)).await,
                _ => return Ok(Status::Unknown),
            }
        }
    })
}
