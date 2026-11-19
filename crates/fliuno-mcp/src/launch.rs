//! Process ownership and transport lifecycle, separate from tool registration.
use super::StdioServerConfig;
#[cfg(all(unix, not(target_os = "linux")))]
use process_wrap::tokio::ProcessGroup;
#[cfg(not(any(windows, target_os = "linux")))]
use process_wrap::tokio::{CommandWrap, KillOnDrop};
#[cfg(not(any(windows, target_os = "linux")))]
use rmcp::transport::TokioChildProcess;
use rmcp::{
    ClientServiceExt, RoleClient,
    service::{RunningService, RxJsonRpcMessage, TxJsonRpcMessage},
    transport::{Transport, async_rw::AsyncRwTransport},
};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, io};
use tokio::task::JoinHandle;

/// Mandatory host configuration; a model cannot select process authority.
#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ProcessIsolation {
    TrustedHost,
    WindowsAppContainer,
    LinuxLandlock,
}

pub(super) fn validate(config: &StdioServerConfig) -> Result<(), String> {
    fliuno_sandbox::validate_staged_files(&config.assets)
        .map_err(|_| "invalid staged MCP files")?;
    if !config.command.is_absolute() || !config.command.is_file() {
        return Err("MCP command must be an absolute existing executable path".into());
    }
    if cfg!(windows)
        && !config
            .command
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("exe"))
    {
        return Err(
            "Windows MCP command must be a native .exe; configure an interpreter explicitly".into(),
        );
    }
    match config.isolation {
        ProcessIsolation::TrustedHost => {
            if !config.assets.is_empty() {
                return Err("staged assets require an isolated MCP server".into());
            }
            if !config
                .cwd
                .as_ref()
                .is_some_and(|cwd| cwd.is_absolute() && cwd.is_dir())
            {
                return Err("trusted_host MCP requires an absolute existing cwd".into());
            }
        }
        ProcessIsolation::WindowsAppContainer | ProcessIsolation::LinuxLandlock => {
            if config.cwd.is_some() {
                return Err("isolated MCP uses private scratch storage; omit cwd".into());
            }
            if config.isolation == ProcessIsolation::WindowsAppContainer && !cfg!(windows) {
                return Err(
                    "windows_app_container is unsupported on this host; no host fallback".into(),
                );
            }
            if config.isolation == ProcessIsolation::LinuxLandlock && !cfg!(target_os = "linux") {
                return Err("linux_landlock is supported only on Linux; no host fallback".into());
            }
        }
    }
    config
        .resources
        .validate()
        .map_err(|_| "invalid or unsupported MCP resource limits")?;
    if let Some(parent) = &config.linux_cgroup_parent
        && (!cfg!(target_os = "linux") || !parent.is_absolute() || !parent.is_dir())
    {
        return Err("linuxCgroupParent must be an existing Linux cgroup directory".into());
    }
    if config.env_allow.len() > 64
        || config
            .env_allow
            .iter()
            .any(|key| key.is_empty() || key.contains(['=', '\0']))
    {
        return Err("invalid MCP envAllow".into());
    }
    Ok(())
}

pub(super) async fn connect(
    config: &StdioServerConfig,
    env: BTreeMap<String, String>,
    executable_digest: String,
) -> Result<RunningService<RoleClient, ()>, String> {
    match config.isolation {
        ProcessIsolation::TrustedHost => {
            #[cfg(any(windows, target_os = "linux"))]
            {
                let (child, pipes) =
                    fliuno_sandbox::spawn_host_async(fliuno_sandbox::HostLaunchSpec {
                        program: config.command.clone(),
                        args: config.args.clone(),
                        env,
                        cwd: config.cwd.as_ref().unwrap().clone(),
                        resources: config.resources,
                        linux_cgroup_parent: config.linux_cgroup_parent.clone(),
                    })
                    .await
                    .map_err(|_| "cannot spawn trusted MCP server")?;
                let transport = ProcessTransport::new(ProcessOwner::Host(child), pipes);
                ().serve_with_lifecycle(transport, config.lifecycle.sdk())
                    .await
                    .map_err(|_| "MCP initialization failed".into())
            }
            #[cfg(not(any(windows, target_os = "linux")))]
            {
                let mut command = CommandWrap::with_new(&config.command, |command| {
                    command
                        .args(&config.args)
                        .current_dir(config.cwd.as_ref().unwrap())
                        .env_clear()
                        .envs(env);
                });
                command.wrap(KillOnDrop);
                #[cfg(unix)]
                command.wrap(ProcessGroup::leader());
                let transport = TokioChildProcess::new(command)
                    .map_err(|_| "cannot spawn trusted MCP server")?;
                ().serve_with_lifecycle(transport, config.lifecycle.sdk())
                    .await
                    .map_err(|_| "MCP initialization failed".into())
            }
        }
        ProcessIsolation::WindowsAppContainer | ProcessIsolation::LinuxLandlock => {
            let spec = fliuno_sandbox::LaunchSpec {
                program_sha256: Some(executable_digest),
                assets: config.assets.clone(),
                program: config.command.clone(),
                args: config.args.clone(),
                env,
                resources: config.resources,
                linux_cgroup_parent: config.linux_cgroup_parent.clone(),
            };
            let (child, pipes) = fliuno_sandbox::spawn_async(spec)
                .await
                .map_err(|error| format!("MCP sandbox launch failed: {error}"))?;
            let transport = ProcessTransport::new(ProcessOwner::Sandbox(child), pipes);
            ().serve_with_lifecycle(transport, config.lifecycle.sdk())
                .await
                .map_err(|_| "isolated MCP initialization failed".into())
        }
    }
}

/// The transport owns the native process, so initialization failure, timeout, cancellation
/// and Drop all close the same process tree. Pipe IO uses the native reactor.
enum ProcessOwner {
    Sandbox(fliuno_sandbox::Child),
    #[cfg(any(windows, target_os = "linux"))]
    Host(fliuno_sandbox::HostChild),
}
impl ProcessOwner {
    fn kill(&mut self) -> io::Result<()> {
        match self {
            Self::Sandbox(child) => child.kill(),
            #[cfg(any(windows, target_os = "linux"))]
            Self::Host(child) => child.kill(),
        }
    }
}
struct ProcessTransport {
    io: AsyncRwTransport<RoleClient, fliuno_sandbox::PipeReader, fliuno_sandbox::PipeWriter>,
    child: ProcessOwner,
    stderr: Option<JoinHandle<io::Result<u64>>>,
}
impl ProcessTransport {
    fn new(child: ProcessOwner, pipes: fliuno_sandbox::AsyncStdio) -> Self {
        let mut errors = pipes.stderr;
        // Drain without storing or displaying potentially sensitive server logs.
        let stderr =
            tokio::spawn(async move { tokio::io::copy(&mut errors, &mut tokio::io::sink()).await });
        Self {
            io: AsyncRwTransport::new(pipes.stdout, pipes.stdin).with_max_line_length(1024 * 1024),
            child,
            stderr: Some(stderr),
        }
    }
}
impl Transport<RoleClient> for ProcessTransport {
    type Error = io::Error;
    fn send(
        &mut self,
        item: TxJsonRpcMessage<RoleClient>,
    ) -> impl Future<Output = io::Result<()>> + Send + 'static {
        self.io.send(item)
    }
    async fn receive(&mut self) -> Option<RxJsonRpcMessage<RoleClient>> {
        self.io.receive().await
    }
    async fn close(&mut self) -> io::Result<()> {
        // Do not flush into a peer which has already stopped reading stdin.
        self.child.kill()?;
        self.io.close().await
    }
}
impl Drop for ProcessTransport {
    fn drop(&mut self) {
        let _ = self.child.kill();
        if let Some(stderr) = self.stderr.take() {
            stderr.abort();
        }
    }
}
