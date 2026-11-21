//! Explicit trusted host commands with native process ownership and reactor pipes.
use crate::{AsyncSpawnError, AsyncSpawnStage, AsyncStdio, LaunchSpec, ResourceLimits};
use std::{collections::BTreeMap, io, path::PathBuf, process::ExitStatus};

/// Explicit host authority. No inherited environment or scratch resource defaults.
pub struct HostLaunchSpec {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub cwd: PathBuf,
    pub resources: ResourceLimits,
    pub linux_cgroup_parent: Option<PathBuf>,
}
impl HostLaunchSpec {
    fn validate(&self) -> io::Result<()> {
        // Reuse the same executable/argument/environment bounds; host mode has
        // no implicit scratch resource limits or environment augmentation.
        LaunchSpec {
            program_sha256: None,
            assets: vec![],
            program: self.program.clone(),
            args: self.args.clone(),
            env: self.env.clone(),
            resources: self.resources,
            linux_cgroup_parent: self.linux_cgroup_parent.clone(),
        }
        .validate()?;
        if !self.cwd.is_absolute() || !self.cwd.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "host cwd must be an absolute existing directory",
            ));
        }
        #[cfg(windows)]
        if !self
            .program
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("exe"))
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "host command must be a native .exe; select an interpreter explicitly",
            ));
        }
        Ok(())
    }
}

/// Owns a trusted program in a Windows Job or Linux process group and optional cgroup.
pub struct HostChild {
    #[cfg(windows)]
    pub(crate) inner: crate::windows::JobProcess,
    #[cfg(target_os = "linux")]
    inner: crate::linux::OwnedProcess,
}
impl HostChild {
    pub fn containment(&self) -> crate::ProcessContainment {
        self.inner.containment()
    }

    pub fn usage(&self) -> crate::ProcessUsage {
        self.inner.usage()
    }

    pub fn id(&self) -> u32 {
        self.inner.id()
    }
    pub fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        self.inner.try_wait()
    }
    pub fn kill(&mut self) -> io::Result<()> {
        self.inner.kill()
    }
}

/// Uses full OS-user authority in the explicit cwd and environment. No PATH,
/// shell or environment inheritance is implicit. Drop cleans up owned processes.
pub async fn spawn_host_async(
    spec: HostLaunchSpec,
) -> Result<(HostChild, AsyncStdio), AsyncSpawnError> {
    let (child, (input, output, error)) = tokio::task::spawn_blocking(move || {
        spec.validate()?;
        #[cfg(windows)]
        {
            crate::windows::host::spawn(&spec)
        }
        #[cfg(target_os = "linux")]
        {
            crate::linux::spawn_host(&spec).map(|(inner, pipes)| (HostChild { inner }, pipes))
        }
    })
    .await
    .map_err(|_| {
        AsyncSpawnError::new(
            AsyncSpawnStage::LaunchWorker,
            io::Error::other("native host launcher worker failed"),
        )
    })?
    .map_err(|error| AsyncSpawnError::new(AsyncSpawnStage::NativeLaunch, error))?;
    let pipes = crate::async_stdio::register_pipes(input, output, error)
        .map_err(|error| AsyncSpawnError::new(AsyncSpawnStage::PipeRegistration, error))?;
    Ok((child, pipes))
}
