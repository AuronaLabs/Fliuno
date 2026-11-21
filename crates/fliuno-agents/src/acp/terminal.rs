//! Run-owned ACP terminals. Peer input selects a preset, never native argv.
use super::{AcpAgentConfig, AgentObservation, AgentObserver, ApprovalDecision, CancellationToken};
use fliuno_acp::{
    Client, HostResponse, MAX_TERMINAL_OUTPUT_BYTES, TerminalRequest, TerminalResponse, schema::*,
};
use fliuno_sandbox::{AsyncStdio, Child, LaunchSpec, ResourceLimits};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{
    collections::{BTreeMap, HashMap},
    path::PathBuf,
    process::ExitStatus,
    time::Duration,
};
use tokio::{io::AsyncReadExt, sync::watch, task::JoinSet, time::Instant};

const MAX_PER_RUN: usize = 4;

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PresetConfig {
    id: String,
    command: PathBuf,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    resources: ResourceLimits,
    #[serde(default = "default_timeout")]
    timeout_ms: u64,
}

fn default_timeout() -> u64 {
    30_000
}

#[derive(Clone)]
pub(super) struct Preset {
    pub(super) spec: LaunchSpec,
    timeout: Duration,
}

pub(super) fn resolve(config: &AcpAgentConfig) -> Result<BTreeMap<String, Preset>, String> {
    if config.terminal_presets.is_empty() {
        return Ok(BTreeMap::new());
    }
    if !matches!(config.isolation, super::Isolation::NativeScratch)
        || !cfg!(any(windows, target_os = "linux"))
        || (cfg!(target_os = "linux") && config.linux_cgroup_parent.is_none())
        || config.terminal_presets.len() > 16
    {
        return Err("ACP terminal presets require native_scratch, up to 16 presets, and an explicit Linux cgroup parent".into());
    }
    let mut presets = BTreeMap::new();
    for preset in &config.terminal_presets {
        if preset.id.is_empty()
            || preset.id.len() > 64
            || !preset
                .id
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'-'))
            || presets.contains_key(&preset.id)
            || !(100..=300_000).contains(&preset.timeout_ms)
            || preset.args.len() > 64
            || preset
                .args
                .iter()
                .any(|arg| arg.contains('\0') || arg.len() > 4096)
            || preset.args.iter().map(String::len).sum::<usize>() > 16 * 1024
            || !preset.command.is_absolute()
            || (cfg!(windows)
                && !preset
                    .command
                    .extension()
                    .is_some_and(|e| e.eq_ignore_ascii_case("exe")))
        {
            return Err("invalid ACP terminal preset".into());
        }
        let spec = LaunchSpec {
            program_sha256: Some(
                fliuno_sandbox::program_digest(&preset.command)
                    .map_err(|_| "cannot pin ACP terminal executable")?,
            ),
            assets: Vec::new(),
            program: preset.command.clone(),
            args: preset.args.clone(),
            env: BTreeMap::new(),
            resources: preset.resources,
            linux_cgroup_parent: config.linux_cgroup_parent.clone(),
        };
        spec.validate()
            .map_err(|_| "invalid ACP terminal launch policy")?;
        presets.insert(
            preset.id.clone(),
            Preset {
                spec,
                timeout: Duration::from_millis(preset.timeout_ms),
            },
        );
    }
    Ok(presets)
}

struct Pending {
    preset: Preset,
    limit: usize,
    cancel: CancellationToken,
    starting: bool,
}

#[derive(Clone)]
struct Running {
    stop: CancellationToken,
    output: watch::Receiver<TerminalOutputResponse>,
}

pub(super) enum Job {
    Launch {
        request: String,
        result: Option<(Box<Child>, AsyncStdio)>,
        deadline: Instant,
    },
    Respond {
        request: String,
        result: HostResponse,
    },
}

pub(super) struct Broker<'a> {
    presets: &'a BTreeMap<String, Preset>,
    root: PathBuf,
    cancellation: CancellationToken,
    pending: HashMap<String, Pending>,
    terminals: HashMap<TerminalId, Running>,
    pub(super) jobs: JoinSet<Job>,
    workers: JoinSet<()>,
}

impl<'a> Broker<'a> {
    pub(super) fn new(
        presets: &'a BTreeMap<String, Preset>,
        root: PathBuf,
        cancellation: CancellationToken,
    ) -> Self {
        Self {
            presets,
            root,
            cancellation,
            pending: HashMap::new(),
            terminals: HashMap::new(),
            jobs: JoinSet::new(),
            workers: JoinSet::new(),
        }
    }

    pub(super) fn request(
        &mut self,
        id: String,
        request: TerminalRequest,
        expires_at: u64,
        observer: &dyn AgentObserver,
        client: &Client,
        session: &SessionId,
    ) -> Result<(), ()> {
        // Reap completed workers before counting admission. Released terminals
        // still consume capacity until their containment has stopped.
        while self.workers.try_join_next().is_some() {}
        match request {
            TerminalRequest::Create(request) => {
                let Some(preset) = self.presets.get(&request.command) else {
                    return reply(client, session, &id, HostResponse::Denied);
                };
                if self.pending.len() + self.terminals.len().max(self.workers.len()) >= MAX_PER_RUN
                    || !request.args.is_empty()
                    || !request.env.is_empty()
                    || request.cwd.as_deref().is_some_and(|path| path != self.root)
                    || request.output_byte_limit == Some(0)
                {
                    return reply(client, session, &id, HostResponse::Denied);
                }
                let limit = request
                    .output_byte_limit
                    .unwrap_or(MAX_TERMINAL_OUTPUT_BYTES as u64)
                    .min(MAX_TERMINAL_OUTPUT_BYTES as u64) as usize;
                observer.observe(AgentObservation::PermissionRequested {
                    permission_id: id.clone(),
                    request: json!({"type":"acp_terminal_create","preset":request.command,
                        "program":preset.spec.program,"sha256":preset.spec.program_sha256,"args":preset.spec.args,
                        "isolation":"native_scratch","cwd":"private","network":"denied","stdin":"closed",
                        "outputBytes":limit,"timeoutMs":preset.timeout.as_millis() as u64,"resources":preset.spec.resources}),
                    allow_once_available: true,
                    expires_at_unix_ms: expires_at,
                }).map_err(|_| ())?;
                self.pending.insert(
                    id,
                    Pending {
                        preset: preset.clone(),
                        limit,
                        starting: false,
                        cancel: self.cancellation.child_token(),
                    },
                );
            }
            TerminalRequest::Output(request) => {
                let result = self.terminals.get(&request.terminal_id).map_or(
                    HostResponse::Failed,
                    |terminal| {
                        HostResponse::Terminal(TerminalResponse::Output(
                            terminal.output.borrow().clone(),
                        ))
                    },
                );
                reply(client, session, &id, result)?;
            }
            TerminalRequest::Wait(request) => {
                self.wait(id, &request.terminal_id, Control::Wait, client, session)?
            }
            TerminalRequest::Kill(request) => {
                self.wait(id, &request.terminal_id, Control::Kill, client, session)?
            }
            TerminalRequest::Release(request) => {
                self.wait(id, &request.terminal_id, Control::Release, client, session)?
            }
        }
        Ok(())
    }

    pub(super) fn approve(
        &mut self,
        id: String,
        decision: ApprovalDecision,
        client: &Client,
        session: &SessionId,
    ) -> Result<(), ()> {
        let Some(pending) = self.pending.get_mut(&id).filter(|p| !p.starting) else {
            return Ok(());
        };
        if decision == ApprovalDecision::Deny {
            return reply(client, session, &id, HostResponse::Denied);
        }
        let Ok(remaining) = client.host_request_remaining(session, &id) else {
            return Ok(());
        };
        pending.starting = true;
        let cancel = pending.cancel.clone();
        let preset = pending.preset.clone();
        self.jobs.spawn(async move {
            let deadline = Instant::now() + preset.timeout;
            let result = tokio::select! {
                biased;
                _ = cancel.cancelled() => None,
                result = tokio::time::timeout(remaining.min(preset.timeout), fliuno_sandbox::spawn_async(preset.spec)) => {
                    result.ok().and_then(Result::ok).map(|(child, pipes)| (Box::new(child), pipes))
                }
            };
            Job::Launch { request: id, result, deadline }
        });
        Ok(())
    }

    pub(super) fn completed(
        &mut self,
        job: Job,
        client: &Client,
        session: &SessionId,
    ) -> Result<(), ()> {
        match job {
            Job::Respond { request, result } => reply(client, session, &request, result),
            Job::Launch {
                request,
                result,
                deadline,
            } => {
                let Some(pending) = self.pending.get(&request) else {
                    return Ok(());
                };
                let Some((child, pipes)) = result else {
                    return reply(client, session, &request, HostResponse::Failed);
                };
                let id = TerminalId::new(format!("terminal_{request}"));
                let stop = self.cancellation.child_token();
                let (output, receiver) = watch::channel(TerminalOutputResponse::new("", false));
                let response = HostResponse::Terminal(TerminalResponse::Created(
                    CreateTerminalResponse::new(id.clone()),
                ));
                match client.respond_host_request(session, &request, response) {
                    Ok(()) => {
                        self.workers.spawn(run(
                            child,
                            pipes,
                            pending.limit,
                            deadline,
                            stop.clone(),
                            output,
                        ));
                        self.terminals.insert(
                            id,
                            Running {
                                stop,
                                output: receiver,
                            },
                        );
                        Ok(())
                    }
                    Err(fliuno_acp::Error::NotFound) => Ok(()),
                    Err(_) => Err(()),
                }
            }
        }
    }

    pub(super) fn closed(&mut self, id: &str, observer: &dyn AgentObserver) -> Result<bool, ()> {
        let Some(pending) = self.pending.remove(id) else {
            return Ok(false);
        };
        pending.cancel.cancel();
        observer
            .observe(AgentObservation::PermissionClosed {
                permission_id: id.into(),
            })
            .map_err(|_| ())?;
        Ok(true)
    }

    fn wait(
        &mut self,
        request: String,
        id: &TerminalId,
        control: Control,
        client: &Client,
        session: &SessionId,
    ) -> Result<(), ()> {
        let Ok(remaining) = client.host_request_remaining(session, &request) else {
            return Ok(());
        };
        let terminal = if matches!(control, Control::Release) {
            self.terminals.remove(id)
        } else {
            self.terminals.get(id).cloned()
        };
        let Some(mut terminal) = terminal else {
            return reply(client, session, &request, HostResponse::Failed);
        };
        if !matches!(control, Control::Wait) {
            terminal.stop.cancel();
        }
        self.jobs.spawn(async move {
            let exited = tokio::time::timeout(
                remaining,
                terminal.output.wait_for(|o| o.exit_status.is_some()),
            )
            .await;
            let result = match exited {
                Ok(Ok(snapshot)) => HostResponse::Terminal(match control {
                    Control::Wait => TerminalResponse::Exited(WaitForTerminalExitResponse::new(
                        snapshot.exit_status.clone().expect("exit predicate"),
                    )),
                    Control::Kill => TerminalResponse::Killed(KillTerminalResponse::new()),
                    Control::Release => TerminalResponse::Released(ReleaseTerminalResponse::new()),
                }),
                _ => HostResponse::Failed,
            };
            Job::Respond { request, result }
        });
        Ok(())
    }

    pub(super) async fn shutdown(mut self) {
        self.cancellation.cancel();
        self.jobs.abort_all();
        while self.jobs.join_next().await.is_some() {}
        while self.workers.join_next().await.is_some() {}
    }
}

enum Control {
    Wait,
    Kill,
    Release,
}

fn reply(client: &Client, session: &SessionId, id: &str, response: HostResponse) -> Result<(), ()> {
    match client.respond_host_request(session, id, response) {
        Ok(()) | Err(fliuno_acp::Error::NotFound) => Ok(()),
        Err(_) => Err(()),
    }
}

struct Tail {
    bytes: Vec<u8>,
    limit: usize,
    truncated: bool,
}

impl Tail {
    fn append(&mut self, data: &[u8]) {
        self.bytes.extend_from_slice(data);
        let remove = self.bytes.len().saturating_sub(self.limit);
        if remove != 0 {
            self.truncated = true;
            self.bytes.drain(..remove);
        }
    }

    fn snapshot(&self) -> TerminalOutputResponse {
        let mut bytes = self.bytes.as_slice();
        if self.truncated {
            while bytes.first().is_some_and(|byte| byte & 0xc0 == 0x80) {
                bytes = &bytes[1..];
            }
        }
        let text = String::from_utf8_lossy(bytes);
        let mut start = text.len().saturating_sub(self.limit);
        while !text.is_char_boundary(start) {
            start += 1;
        }
        TerminalOutputResponse::new(&text[start..], self.truncated || start > 0)
    }
}

async fn run(
    mut child: Box<Child>,
    pipes: AsyncStdio,
    limit: usize,
    deadline: Instant,
    stop: CancellationToken,
    output: watch::Sender<TerminalOutputResponse>,
) {
    let AsyncStdio {
        stdin,
        mut stdout,
        mut stderr,
    } = pipes;
    drop(stdin);
    let mut tail = Tail {
        bytes: Vec::new(),
        limit,
        truncated: false,
    };
    let mut interval = tokio::time::interval(Duration::from_millis(20));
    let mut left = [0; 8192];
    let mut right = [0; 8192];
    let (mut read_left, mut read_right) = (true, true);
    let result = loop {
        tokio::select! {
            _ = stop.cancelled() => break Ok(None),
            _ = tokio::time::sleep_until(deadline) => break Ok(None),
            _ = interval.tick() => match child.try_wait() {
                Ok(None) => {},
                result => break result,
            },
            count = stdout.read(&mut left), if read_left => match count {
                Ok(0) | Err(_) => read_left = false,
                Ok(n) => { tail.append(&left[..n]); output.send_replace(tail.snapshot()); },
            },
            count = stderr.read(&mut right), if read_right => match count {
                Ok(0) | Err(_) => read_right = false,
                Ok(n) => { tail.append(&right[..n]); output.send_replace(tail.snapshot()); },
            },
        }
    };
    let Ok(Ok(stopped)) = tokio::task::spawn_blocking(move || {
        child.terminate_tree()?;
        child.try_wait()
    })
    .await
    else {
        return;
    };
    let Ok(status) = result else {
        return;
    };
    let _ = tokio::time::timeout(Duration::from_millis(250), async {
        while read_left || read_right {
            tokio::select! {
                count = stdout.read(&mut left), if read_left => match count {
                    Ok(0) | Err(_) => read_left = false,
                    Ok(n) => tail.append(&left[..n]),
                },
                count = stderr.read(&mut right), if read_right => match count {
                    Ok(0) | Err(_) => read_right = false,
                    Ok(n) => tail.append(&right[..n]),
                },
            }
        }
    })
    .await;
    output.send_replace(tail.snapshot().exit_status(exit_status(status.or(stopped))));
}

fn exit_status(status: Option<ExitStatus>) -> TerminalExitStatus {
    let Some(status) = status else {
        return TerminalExitStatus::new();
    };
    if let Some(code) = status.code() {
        return TerminalExitStatus::new().exit_code(code as u32);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(signal) = status.signal() {
            if signal == 9 {
                return TerminalExitStatus::new().signal("SIGKILL");
            }
            return TerminalExitStatus::new().signal(format!("SIG{signal}"));
        }
    }
    TerminalExitStatus::new()
}
