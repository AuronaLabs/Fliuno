//! Reactor-backed native stdio. File IO's blocking pool is unsuitable for a
//! pipe whose peer may stay alive when the owning future is cancelled.
use crate::{Child, LaunchSpec, PrivateState};
use std::{fmt, io};
use tokio::io::{AsyncRead, AsyncWrite};

pub type PipeReader = Box<dyn AsyncRead + Unpin + Send>;
pub type PipeWriter = Box<dyn AsyncWrite + Unpin + Send>;

/// Parent endpoints only. Child endpoints are inherited during native launch;
/// no pipe names, raw descriptors or handles cross the public API.
pub struct AsyncStdio {
    pub stdin: PipeWriter,
    pub stdout: PipeReader,
    pub stderr: PipeReader,
}

/// A post-launch pipe-registration error cannot be treated as proof that the
/// native program never ran. Keep the phase available without displaying paths.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AsyncSpawnStage {
    NativeLaunch,
    LaunchWorker,
    PipeRegistration,
}

#[derive(Debug)]
pub struct AsyncSpawnError {
    pub stage: AsyncSpawnStage,
    source: io::Error,
}
impl AsyncSpawnError {
    pub(crate) fn new(stage: AsyncSpawnStage, source: io::Error) -> Self {
        Self { stage, source }
    }
}
impl fmt::Display for AsyncSpawnError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "native process {:?} failed", self.stage)
    }
}
impl std::error::Error for AsyncSpawnError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

/// Stage and launch on a blocking worker, then register its private parent pipe
/// endpoints with the caller's IO runtime. Dropping a pending launch detaches
/// the worker, whose eventual Child result is dropped and kills the owned tree.
/// Native startup itself remains synchronous and cannot be preempted.
pub async fn spawn_async(spec: LaunchSpec) -> Result<(Child, AsyncStdio), AsyncSpawnError> {
    spawn_async_inner(spec, None).await
}

pub async fn spawn_async_with_state(
    spec: LaunchSpec,
    state: PrivateState,
) -> Result<(Child, AsyncStdio), AsyncSpawnError> {
    spawn_async_inner(spec, Some(state)).await
}

async fn spawn_async_inner(
    spec: LaunchSpec,
    state: Option<PrivateState>,
) -> Result<(Child, AsyncStdio), AsyncSpawnError> {
    let mut child = tokio::task::spawn_blocking(move || {
        spec.validate()?;
        #[cfg(windows)]
        {
            crate::windows::spawn_async_pipes(&spec, state)
        }
        #[cfg(not(windows))]
        {
            crate::spawn_inner(&spec, state)
        }
    })
    .await
    .map_err(|_| {
        AsyncSpawnError::new(
            AsyncSpawnStage::LaunchWorker,
            io::Error::other("native launcher worker failed"),
        )
    })?
    .map_err(|error| AsyncSpawnError::new(AsyncSpawnStage::NativeLaunch, error))?;
    let pipes = register(&mut child)
        .map_err(|error| AsyncSpawnError::new(AsyncSpawnStage::PipeRegistration, error))?;
    Ok((child, pipes))
}

fn register(child: &mut Child) -> io::Result<AsyncStdio> {
    let missing = || io::Error::other("isolated process pipe missing");
    let input = child.stdin.take().ok_or_else(missing)?;
    let output = child.stdout.take().ok_or_else(missing)?;
    let error = child.stderr.take().ok_or_else(missing)?;
    register_pipes(input, output, error)
}

pub(crate) fn register_pipes(
    input: std::fs::File,
    output: std::fs::File,
    error: std::fs::File,
) -> io::Result<AsyncStdio> {
    #[cfg(windows)]
    let pipes = crate::windows::async_io::register(input, output, error)?;
    #[cfg(target_os = "linux")]
    let pipes = AsyncStdio {
        stdin: Box::new(tokio::net::unix::pipe::Sender::from_file(input)?),
        stdout: Box::new(tokio::net::unix::pipe::Receiver::from_file(output)?),
        stderr: Box::new(tokio::net::unix::pipe::Receiver::from_file(error)?),
    };
    #[cfg(not(any(windows, target_os = "linux")))]
    {
        let _ = (input, output, error);
        Err(crate::unsupported())
    }
    #[cfg(any(windows, target_os = "linux"))]
    Ok(pipes)
}
