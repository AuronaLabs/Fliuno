//! Native process isolation, independent of task state, providers and protocols.
//!
//! Windows uses a fresh AppContainer; Linux uses Landlock and seccomp. Both
//! stage one executable and explicit pinned companions in private storage, failing closed when required
//! OS features are unavailable. Platform-specific limits are documented in
//! `docs/sandbox.md`.
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, fs::File, io, path::PathBuf, process::ExitStatus};
mod assets;
mod program;
mod state;
pub use assets::{
    MAX_ASSET_BYTES, MAX_BUNDLE_BYTES, MAX_BUNDLE_FILES, StagedFile, staged_files_digest,
    validate_staged_files,
};
pub use program::{
    MAX_PROGRAM_BYTES, MAX_TRUSTED_PROGRAM_BYTES, program_digest, trusted_program_digest,
};
pub use state::PrivateState;

#[cfg(feature = "async-io")]
mod async_stdio;
#[cfg(feature = "async-io")]
pub use async_stdio::{
    AsyncSpawnError, AsyncSpawnStage, AsyncStdio, PipeReader, PipeWriter, spawn_async,
    spawn_async_with_state,
};
#[cfg(all(any(windows, target_os = "linux"), feature = "async-io"))]
mod host;
#[cfg(all(any(windows, target_os = "linux"), feature = "async-io"))]
pub use host::{HostChild, HostLaunchSpec, spawn_host_async};

#[cfg(target_os = "linux")]
#[allow(unsafe_code)]
mod linux;
#[cfg(windows)]
#[allow(unsafe_code)]
mod windows;

/// Create a new private directory without inheriting a permissive parent ACL.
/// An existing path is an error: callers must never weaken another directory.
pub fn create_private_directory(path: &std::path::Path) -> io::Result<()> {
    #[cfg(windows)]
    {
        windows::service::create_private_directory(path)
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new().mode(0o700).create(path)
    }
    #[cfg(not(any(windows, unix)))]
    {
        let _ = path;
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "private directories unsupported",
        ))
    }
}

/// Dedicated workers only. Installs an inheritable Windows Job *before* they
/// spawn children. The OS retains its handle until process exit; closing the
/// last handle kills the worker and all descendants, including on hard crash.
/// This controls lifetime, not filesystem/network authority.
#[cfg(windows)]
pub fn contain_service_process(resources: ResourceLimits) -> io::Result<()> {
    windows::service::contain_current_process(resources)
}

/// Read-only diagnostic. A PID can be reused; this result must never authorize
/// killing or attaching to a process. Used by native lifecycle probes.
#[cfg(windows)]
pub fn process_is_running(pid: u32) -> io::Result<bool> {
    windows::service::process_is_running(pid)
}

/// Independent trusted worker. Retain this handle in a dedicated waiter until
/// the worker exits. Host process exit does not end a successfully launched
/// worker; Linux explicit handle Drop instead tears down the owned tree, while
/// Windows Drop closes only the observation handle. Only explicit stdio handles
/// cross the Windows spawn boundary, so a worker cannot retain host RPC pipes.
pub struct ServiceProcess {
    pub stdin: Option<File>,
    #[cfg(windows)]
    inner: windows::service::Detached,
    #[cfg(target_os = "linux")]
    inner: linux::ServiceProcess,
    #[cfg(all(unix, not(target_os = "linux")))]
    inner: std::process::Child,
}
impl ServiceProcess {
    pub fn wait(&mut self) -> io::Result<()> {
        #[cfg(windows)]
        {
            self.inner.wait()
        }
        #[cfg(target_os = "linux")]
        {
            self.inner.wait()
        }
        #[cfg(all(unix, not(target_os = "linux")))]
        {
            self.inner.wait().map(|_| ())
        }
        #[cfg(not(any(windows, unix)))]
        {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "service processes unsupported",
            ))
        }
    }
}
pub fn spawn_service(program: &std::path::Path, args: &[String]) -> io::Result<ServiceProcess> {
    spawn_service_with_limits(program, args, ResourceLimits::default(), None)
}

/// Spawn an independent host worker with an optional Linux cgroup owner.
/// Linux applies limits and installs an independent worker-pidfd guardian before
/// exec when cgroup v2 is available. Windows service workers
/// must call contain_service_process with the same limits before spawning children.
pub fn spawn_service_with_limits(
    program: &std::path::Path,
    args: &[String],
    resources: ResourceLimits,
    linux_cgroup_parent: Option<&std::path::Path>,
) -> io::Result<ServiceProcess> {
    let mut env = std::collections::BTreeMap::new();
    if cfg!(windows)
        && let Ok(root) = std::env::var("SystemRoot")
    {
        env.insert("SystemRoot".into(), root);
    }
    let spec = LaunchSpec {
        program_sha256: None,
        assets: vec![],
        program: program.into(),
        args: args.to_vec(),
        env,
        resources,
        linux_cgroup_parent: linux_cgroup_parent.map(std::path::Path::to_path_buf),
    };
    spec.validate()?;
    #[cfg(windows)]
    {
        windows::service::spawn(&spec)
    }
    #[cfg(target_os = "linux")]
    {
        linux::spawn_service(&spec)
    }
    #[cfg(all(unix, not(target_os = "linux")))]
    {
        use std::{
            os::unix::process::CommandExt,
            process::{Command, Stdio},
        };
        let mut child = Command::new(program)
            .args(args)
            .env_clear()
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()?;
        Ok(ServiceProcess {
            stdin: child
                .stdin
                .take()
                .map(|input| File::from(std::os::fd::OwnedFd::from(input))),
            inner: child,
        })
    }
    #[cfg(not(any(windows, unix)))]
    {
        let _ = spec;
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "service processes unsupported",
        ))
    }
}

/// Host-selected native program. No shell or environment is inherited implicitly.
#[derive(Clone)]
pub struct LaunchSpec {
    /// Optional host pin verified against the copied executable before startup.
    pub program_sha256: Option<String>,
    /// Host-pinned companions copied into private storage before execution.
    pub assets: Vec<StagedFile>,
    pub program: PathBuf,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
    /// Host-selected kernel resource limits; never supplied by model output.
    pub resources: ResourceLimits,
    /// Optional host-delegated Linux cgroup v2 parent for this process tree.
    pub linux_cgroup_parent: Option<PathBuf>,
}

/// Optional hard process budgets. A requested limit is never silently ignored.
#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ResourceLimits {
    /// Memory ceiling in bytes; accounting details vary by platform.
    #[serde(default)]
    pub memory_max_bytes: Option<u64>,
    /// Maximum number of processes in the isolated process tree.
    #[serde(default)]
    pub pids_max: Option<u32>,
    /// Linux CPU quota in microseconds per fixed 100 ms period.
    #[serde(default)]
    pub cpu_quota_micros: Option<u64>,
}

impl ResourceLimits {
    pub fn is_empty(&self) -> bool {
        self.memory_max_bytes.is_none()
            && self.pids_max.is_none()
            && self.cpu_quota_micros.is_none()
    }

    pub fn validate(&self) -> io::Result<()> {
        if self
            .memory_max_bytes
            .is_some_and(|bytes| !(16 * 1024 * 1024..=1024 * 1024 * 1024 * 1024).contains(&bytes))
            || self
                .pids_max
                .is_some_and(|count| !(1..=4096).contains(&count))
            || self
                .cpu_quota_micros
                .is_some_and(|quota| !(1_000..=100_000_000).contains(&quota))
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "resource limits are outside the supported memory, PID, or CPU range",
            ));
        }
        #[cfg(windows)]
        if self.cpu_quota_micros.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "cpuQuotaMicros is currently supported only by the Linux cgroup v2 backend",
            ));
        }
        #[cfg(not(any(windows, target_os = "linux")))]
        if !self.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "resource limits are unavailable on this platform",
            ));
        }
        Ok(())
    }
}

impl LaunchSpec {
    pub fn validate(&self) -> io::Result<()> {
        if self
            .program_sha256
            .as_ref()
            .is_some_and(|digest| !program::valid_digest(digest))
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "program SHA-256 must be 64 lowercase hexadecimal digits",
            ));
        }
        validate_staged_files(&self.assets)?;
        self.resources.validate()?;
        if self
            .linux_cgroup_parent
            .as_ref()
            .is_some_and(|path| !path.is_absolute() || !path.is_dir())
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "linuxCgroupParent must be an absolute existing directory",
            ));
        }
        #[cfg(not(target_os = "linux"))]
        if self.linux_cgroup_parent.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "linuxCgroupParent is supported only on Linux",
            ));
        }
        if !self.program.is_absolute() || !self.program.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "sandbox program must be an absolute existing native executable",
            ));
        }
        if self.args.len() > 256
            || self.args.iter().any(|arg| arg.contains('\0'))
            || self.args.iter().map(String::len).sum::<usize>() > 32_000
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid sandbox arguments",
            ));
        }
        let mut keys = std::collections::BTreeSet::new();
        if self
            .env
            .iter()
            .map(|(key, value)| key.len() + value.len())
            .sum::<usize>()
            > 256 * 1024
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "sandbox environment exceeds 256 KiB",
            ));
        }
        for (key, value) in &self.env {
            if key.is_empty()
                || !key.is_ascii()
                || key.contains(['=', '\0'])
                || value.contains('\0')
                || !keys.insert(key.to_ascii_uppercase())
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "invalid sandbox environment",
                ));
            }
        }
        Ok(())
    }
}

/// Measurements from a launcher-owned process tree when the OS exposes them.
#[derive(Debug, Default, Clone, Copy)]
pub struct ProcessUsage {
    pub cpu_ms: Option<u64>,
    pub peak_memory_bytes: Option<u64>,
}

/// Owns the isolated process tree. Drop kills it before removing private storage.
pub struct Child {
    pub stdin: Option<File>,
    pub stdout: Option<File>,
    pub stderr: Option<File>,
    state: Option<Box<state::PreparedState>>,
    #[cfg(windows)]
    inner: windows::Process,
    #[cfg(target_os = "linux")]
    inner: linux::Process,
}

pub fn spawn(spec: &LaunchSpec) -> io::Result<Child> {
    spawn_inner(spec, None)
}

/// Launch with an optional restored state directory and an immutable next slot.
pub fn spawn_with_state(spec: &LaunchSpec, state: PrivateState) -> io::Result<Child> {
    spawn_inner(spec, Some(state))
}

fn spawn_inner(spec: &LaunchSpec, state: Option<PrivateState>) -> io::Result<Child> {
    spec.validate()?;
    #[cfg(windows)]
    {
        windows::spawn(spec, state)
    }
    #[cfg(target_os = "linux")]
    {
        linux::spawn(spec, state)
    }
    #[cfg(not(any(windows, target_os = "linux")))]
    {
        let _ = state;
        Err(unsupported())
    }
}

impl Child {
    pub fn state_directory(&self) -> Option<PathBuf> {
        self.state.as_ref().map(|state| state.home.clone())
    }

    /// Stop the owned process before copying data to the host checkpoint slot.
    pub fn checkpoint(&mut self) -> io::Result<()> {
        let state = self.state.take().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "launch has no private state")
        })?;
        self.kill()?;
        #[cfg(windows)]
        self.inner.wait_for_empty_job()?;
        state.capture()
    }

    pub fn usage(&self) -> ProcessUsage {
        #[cfg(any(windows, target_os = "linux"))]
        {
            self.inner.usage()
        }
        #[cfg(not(any(windows, target_os = "linux")))]
        {
            ProcessUsage::default()
        }
    }

    pub fn id(&self) -> u32 {
        #[cfg(windows)]
        {
            self.inner.id()
        }
        #[cfg(target_os = "linux")]
        {
            self.inner.id()
        }
        #[cfg(not(any(windows, target_os = "linux")))]
        {
            0
        }
    }

    pub fn private_directory(&self) -> &std::path::Path {
        #[cfg(windows)]
        {
            self.inner.directory()
        }
        #[cfg(target_os = "linux")]
        {
            self.inner.directory()
        }
        #[cfg(not(any(windows, target_os = "linux")))]
        {
            std::path::Path::new("")
        }
    }

    /// Whether a delegated Linux cgroup v2 subtree owns this process tree.
    /// Other platforms and Linux hosts without delegation return `false`.
    pub fn uses_cgroup_v2(&self) -> bool {
        #[cfg(target_os = "linux")]
        {
            self.inner.uses_cgroup_v2()
        }
        #[cfg(not(target_os = "linux"))]
        {
            false
        }
    }

    pub fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        #[cfg(windows)]
        {
            self.inner.try_wait()
        }
        #[cfg(target_os = "linux")]
        {
            self.inner.try_wait()
        }
        #[cfg(not(any(windows, target_os = "linux")))]
        {
            Err(unsupported())
        }
    }

    pub fn kill(&mut self) -> io::Result<()> {
        #[cfg(windows)]
        {
            self.inner.kill()
        }
        #[cfg(target_os = "linux")]
        {
            self.inner.kill()
        }
        #[cfg(not(any(windows, target_os = "linux")))]
        {
            Err(unsupported())
        }
    }
}

/// Token diagnostic used by probes; it does not decide whether to trust a peer.
pub fn current_process_is_app_container() -> io::Result<bool> {
    #[cfg(windows)]
    {
        windows::current_process_is_app_container()
    }
    #[cfg(not(windows))]
    {
        Err(unsupported())
    }
}

/// Kernel-backed diagnostic for the active native sandbox.
pub fn current_process_is_isolated() -> io::Result<bool> {
    #[cfg(windows)]
    {
        current_process_is_app_container()
    }
    #[cfg(target_os = "linux")]
    {
        linux::current_process_is_isolated()
    }
    #[cfg(not(any(windows, target_os = "linux")))]
    {
        Err(unsupported())
    }
}

#[cfg(not(windows))]
fn unsupported() -> io::Error {
    io::Error::new(
        io::ErrorKind::Unsupported,
        "native scratch isolation is unavailable on this host; no host fallback",
    )
}
