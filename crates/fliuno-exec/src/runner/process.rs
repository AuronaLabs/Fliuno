//! Host/isolated ownership and launch policy. The runner drains streams and
//! records lifecycle evidence without containing platform-specific spawn code.
use crate::{Arguments, Program, config::ProcessIsolation};
use fliuno_core::{ToolContext, ToolError};
use fliuno_sandbox::{PipeReader, PipeWriter};
#[cfg(unix)]
use process_wrap::tokio::ProcessGroup;
#[cfg(unix)]
use process_wrap::tokio::{ChildWrapper, CommandWrap, KillOnDrop};
#[cfg(unix)]
use std::process::Stdio;
use std::{io, process::ExitStatus};

pub(super) enum Process {
    #[cfg(unix)]
    Host(Box<dyn ChildWrapper>),
    #[cfg(windows)]
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
    fn try_wait(&mut self) -> io::Result<Option<ExitStatus>>;
    fn stop(&mut self) -> io::Result<()>;
    fn pipes(&mut self) -> io::Result<(PipeWriter, PipeReader, PipeReader)>;
    fn stop_found_absent_group(&self, _root_reaped: bool, _error: &io::Error) -> bool {
        false
    }
}
impl ProcessControl for Process {
    fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        match self {
            #[cfg(unix)]
            Self::Host(child) => child.try_wait(),
            #[cfg(windows)]
            Self::NativeHost(child, _) => child.try_wait(),
            Self::Scratch(child, _) => child.try_wait(),
        }
    }
    fn stop(&mut self) -> io::Result<()> {
        match self {
            #[cfg(unix)]
            Self::Host(child) => child.start_kill(),
            #[cfg(windows)]
            Self::NativeHost(child, _) => child.kill(),
            Self::Scratch(child, _) => child.kill(),
        }
    }
    fn stop_found_absent_group(&self, root_reaped: bool, error: &io::Error) -> bool {
        // Only ESRCH from our Unix host process group proves that there is no
        // group left to signal. A Windows Job or other sandbox cleanup failure
        // must stay unknown even when the root's exit status is available.
        #[cfg(unix)]
        {
            root_reaped
                && matches!(self, Self::Host(_))
                && error.raw_os_error() == Some(libc::ESRCH)
        }
        #[cfg(not(unix))]
        {
            let _ = (root_reaped, error);
            false
        }
    }
    fn pipes(&mut self) -> io::Result<(PipeWriter, PipeReader, PipeReader)> {
        let missing = || io::Error::other("missing process pipe");
        Ok(match self {
            #[cfg(unix)]
            Self::Host(child) => (
                Box::new(child.stdin().take().ok_or_else(missing)?),
                Box::new(child.stdout().take().ok_or_else(missing)?),
                Box::new(child.stderr().take().ok_or_else(missing)?),
            ),
            Self::Scratch(_, pipes) => {
                let pipes = pipes.take().ok_or_else(missing)?;
                (pipes.stdin, pipes.stdout, pipes.stderr)
            }
            #[cfg(windows)]
            Self::NativeHost(_, pipes) => {
                let pipes = pipes.take().ok_or_else(missing)?;
                (pipes.stdin, pipes.stdout, pipes.stderr)
            }
        })
    }
}
impl Process {
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
                        fliuno_sandbox::AsyncSpawnStage::NativeLaunch => ToolError::Failed("cannot launch isolated program".into()),
                        _ => ToolError::OutcomeUnknown("isolated launcher worker or pipe registration failed after native launch may have run; inspect effects before retrying".into()),
                    })
            }
            ProcessIsolation::TrustedHost => {
                #[cfg(windows)]
                {
                    fliuno_sandbox::spawn_host_async(fliuno_sandbox::HostLaunchSpec {
                        program: program.config.command.clone(), args: argv,
                        env: program.environment.clone(), cwd: context.workspace.root.clone().into(),
                        resources: Default::default(),
                        linux_cgroup_parent: None,
                    }).await.map(|(child, pipes)| Self::NativeHost(Box::new(child), Some(pipes)))
                        .map_err(|error| match error.stage {
                            fliuno_sandbox::AsyncSpawnStage::NativeLaunch => ToolError::Failed("cannot launch host program".into()),
                            _ => ToolError::OutcomeUnknown("host launcher worker or pipe registration failed after native launch may have run; inspect effects before retrying".into()),
                        })
                }
                #[cfg(unix)]
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
