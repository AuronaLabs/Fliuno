//! Host/isolated ownership and launch policy. The runner drains streams and
//! records lifecycle evidence without containing platform-specific spawn code.
use crate::{Arguments, Program, config::ProcessIsolation};
use fliuno_core::{ToolContext, ToolError};
use fliuno_sandbox::{PipeReader, PipeWriter};
#[cfg(all(unix, not(target_os = "linux")))]
use process_wrap::tokio::ProcessGroup;
#[cfg(all(unix, not(target_os = "linux")))]
use process_wrap::tokio::{ChildWrapper, CommandWrap, KillOnDrop};
#[cfg(all(unix, not(target_os = "linux")))]
use std::process::Stdio;
use std::{io, process::ExitStatus};

pub(super) enum Process {
    #[cfg(all(unix, not(target_os = "linux")))]
    Host(Box<dyn ChildWrapper>),
    #[cfg(any(windows, target_os = "linux"))]
    NativeHost(
        Box<fliuno_sandbox::HostChild>,
        Option<fliuno_sandbox::AsyncStdio>,
    ),
    Scratch(
        Box<fliuno_sandbox::Child>,
        Option<fliuno_sandbox::AsyncStdio>,
    ),
}
/// The lifecycle driver needs only owned process operations and stream IO.
/// Platform launch stays concrete; failure scheduling can be tested separately.
pub(super) trait ProcessControl {
    fn usage(&self) -> fliuno_sandbox::ProcessUsage {
        fliuno_sandbox::ProcessUsage::default()
    }
    fn try_wait(&mut self) -> io::Result<Option<ExitStatus>>;
    fn stop(&mut self) -> io::Result<()>;
    fn pipes(&mut self) -> io::Result<(PipeWriter, PipeReader, PipeReader)>;
    fn stop_found_absent_group(&self, _root_reaped: bool, _error: &io::Error) -> bool {
        false
    }
}
impl<T: ProcessControl> ProcessControl for &mut T {
    fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        (**self).try_wait()
    }
    fn stop(&mut self) -> io::Result<()> {
        (**self).stop()
    }
    fn pipes(&mut self) -> io::Result<(PipeWriter, PipeReader, PipeReader)> {
        (**self).pipes()
    }
    fn stop_found_absent_group(&self, root_reaped: bool, error: &io::Error) -> bool {
        (**self).stop_found_absent_group(root_reaped, error)
    }
}

impl ProcessControl for Process {
    fn usage(&self) -> fliuno_sandbox::ProcessUsage {
        match self {
            #[cfg(all(unix, not(target_os = "linux")))]
            Self::Host(_) => fliuno_sandbox::ProcessUsage::default(),
            #[cfg(any(windows, target_os = "linux"))]
            Self::NativeHost(child, _) => child.usage(),
            Self::Scratch(child, _) => child.usage(),
        }
    }

    fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        match self {
            #[cfg(all(unix, not(target_os = "linux")))]
            Self::Host(child) => child.try_wait(),
            #[cfg(any(windows, target_os = "linux"))]
            Self::NativeHost(child, _) => child.try_wait(),
            Self::Scratch(child, _) => child.try_wait(),
        }
    }
    fn stop(&mut self) -> io::Result<()> {
        match self {
            #[cfg(all(unix, not(target_os = "linux")))]
            Self::Host(child) => child.start_kill(),
            #[cfg(any(windows, target_os = "linux"))]
            Self::NativeHost(child, _) => child.kill(),
            Self::Scratch(child, _) => child.kill(),
        }
    }
    fn stop_found_absent_group(&self, root_reaped: bool, error: &io::Error) -> bool {
        // Only ESRCH from our Unix host process group proves that there is no
        // group left to signal. A Windows Job or other sandbox cleanup failure
        // must stay unknown even when the root's exit status is available.
        #[cfg(all(unix, not(target_os = "linux")))]
        {
            root_reaped
                && matches!(self, Self::Host(_))
                && error.raw_os_error() == Some(libc::ESRCH)
        }
        #[cfg(not(all(unix, not(target_os = "linux"))))]
        {
            let _ = (root_reaped, error);
            false
        }
    }
    fn pipes(&mut self) -> io::Result<(PipeWriter, PipeReader, PipeReader)> {
        #[cfg(all(unix, not(target_os = "linux")))]
        let missing = || io::Error::other("missing process pipe");
        Ok(match self {
            #[cfg(all(unix, not(target_os = "linux")))]
            Self::Host(child) => (
                Box::new(child.stdin().take().ok_or_else(missing)?),
                Box::new(child.stdout().take().ok_or_else(missing)?),
                Box::new(child.stderr().take().ok_or_else(missing)?),
            ),
            Self::Scratch(_, pipes) => take_native_pipes(pipes)?,
            #[cfg(any(windows, target_os = "linux"))]
            Self::NativeHost(_, pipes) => take_native_pipes(pipes)?,
        })
    }
}
fn take_native_pipes(
    pipes: &mut Option<fliuno_sandbox::AsyncStdio>,
) -> io::Result<(PipeWriter, PipeReader, PipeReader)> {
    let pipes = pipes
        .take()
        .ok_or_else(|| io::Error::other("missing process pipe"))?;
    Ok((pipes.stdin, pipes.stdout, pipes.stderr))
}

impl Process {
    pub(super) fn containment(&self) -> Option<fliuno_sandbox::ProcessContainment> {
        match self {
            #[cfg(all(unix, not(target_os = "linux")))]
            Self::Host(_) => Some(fliuno_sandbox::ProcessContainment {
                tree_backend: fliuno_sandbox::TreeBackend::ProcessGroup,
                limits: Some(fliuno_sandbox::ResourceLimits::default()),
            }),
            #[cfg(any(windows, target_os = "linux"))]
            Self::NativeHost(child, _) => Some(child.containment()),
            Self::Scratch(child, _) => child.containment(),
        }
    }

    fn launch_rejected(program: &Program, isolated: bool) -> ToolError {
        let scope = if isolated { "isolated" } else { "host" };
        let requirement = if !program.config.resources.is_empty()
            || program.config.linux_cgroup_parent.is_some()
        {
            " with the required process containment and resource limits"
        } else {
            ""
        };
        ToolError::Failed(format!("cannot launch {scope} program{requirement}"))
    }

    pub(super) async fn spawn(
        program: &Program,
        context: &ToolContext<'_>,
        args: &Arguments,
    ) -> Result<Self, ToolError> {
        let argv = program
            .config
            .args
            .iter()
            .chain(&args.args)
            .cloned()
            .collect::<Vec<_>>();
        match program.config.isolation {
            ProcessIsolation::WindowsAppContainer | ProcessIsolation::LinuxLandlock => {
                let spec = fliuno_sandbox::LaunchSpec {
                    program_sha256: Some(program.executable_digest.clone()),
                    assets: program.config.assets.clone(),
                    program: program.config.command.clone(),
                    args: argv,
                    env: program.environment.clone(),
                    resources: program.config.resources,
                    linux_cgroup_parent: program.config.linux_cgroup_parent.clone(),
                };
                fliuno_sandbox::spawn_async(spec)
                    .await
                    .map(|(child, pipes)| Self::Scratch(Box::new(child), Some(pipes)))
                    .map_err(|error| match error.stage {
                        fliuno_sandbox::AsyncSpawnStage::NativeLaunch => Self::launch_rejected(program, true),
                        _ => ToolError::OutcomeUnknown("isolated launcher worker or pipe registration failed after native launch may have run; inspect effects before retrying".into()),
                    })
            }
            ProcessIsolation::TrustedHost => {
                #[cfg(any(windows, target_os = "linux"))]
                {
                    fliuno_sandbox::spawn_host_async(fliuno_sandbox::HostLaunchSpec {
                        program: program.config.command.clone(), args: argv,
                        env: program.environment.clone(), cwd: context.workspace.root.clone().into(),
                        resources: program.config.resources,
                        linux_cgroup_parent: program.config.linux_cgroup_parent.clone(),
                    }).await.map(|(child, pipes)| Self::NativeHost(Box::new(child), Some(pipes)))
                        .map_err(|error| match error.stage {
                            fliuno_sandbox::AsyncSpawnStage::NativeLaunch => Self::launch_rejected(program, false),
                            _ => ToolError::OutcomeUnknown("host launcher worker or pipe registration failed after native launch may have run; inspect effects before retrying".into()),
                        })
                }
                #[cfg(all(unix, not(target_os = "linux")))]
                {
                    let mut command = CommandWrap::with_new(&program.config.command, |command| {
                        command
                            .args(argv)
                            .current_dir(&context.workspace.root)
                            .env_clear()
                            .envs(&program.environment)
                            .stdin(Stdio::piped())
                            .stdout(Stdio::piped())
                            .stderr(Stdio::piped());
                    });
                    command.wrap(KillOnDrop);
                    command.wrap(ProcessGroup::leader());
                    command.spawn().map(Self::Host).map_err(|_|ToolError::OutcomeUnknown("host process launch or ownership setup failed; inspect effects before retrying".into()))
                }
                #[cfg(not(any(windows, unix)))]
                {
                    Err(ToolError::Failed(
                        "host process execution unsupported on this platform".into(),
                    ))
                }
            }
        }
    }
}
impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}
