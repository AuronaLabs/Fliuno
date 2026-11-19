//! Bounded newline framing and ownership of native ACP peers.
use agent_client_protocol as acp;
use futures::{SinkExt, StreamExt};
use std::{io, path::PathBuf};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::task::JoinHandle;
use tokio_util::codec::{FramedRead, FramedWrite, LinesCodec};

const MAX_LINE_BYTES: usize = 1024 * 1024;

pub(crate) fn lines(
    input: impl AsyncRead + Send + 'static,
    output: impl AsyncWrite + Send + 'static,
) -> impl acp::ConnectTo<acp::Client> {
    acp::Lines::new(
        SinkExt::<String>::sink_map_err(
            FramedWrite::new(output, LinesCodec::new_with_max_length(MAX_LINE_BYTES)),
            io::Error::other,
        ),
        FramedRead::new(input, LinesCodec::new_with_max_length(MAX_LINE_BYTES))
            .map(|line| line.map_err(io::Error::other)),
    )
}

pub(crate) enum Child {
    Isolated(fliuno_sandbox::Child),
    #[cfg(any(windows, target_os = "linux"))]
    Trusted(fliuno_sandbox::HostChild),
}

pub(crate) struct NativePeer {
    child: Child,
    stderr: Option<JoinHandle<io::Result<u64>>>,
}

impl NativePeer {
    pub fn new(
        child: Child,
        pipes: fliuno_sandbox::AsyncStdio,
    ) -> (
        Self,
        fliuno_sandbox::PipeReader,
        fliuno_sandbox::PipeWriter,
        Option<PathBuf>,
    ) {
        let mut stderr = pipes.stderr;
        let directory = match &child {
            Child::Isolated(child) => Some(child.private_directory().to_owned()),
            #[cfg(any(windows, target_os = "linux"))]
            Child::Trusted(_) => None,
        };
        let stderr =
            tokio::spawn(async move { tokio::io::copy(&mut stderr, &mut tokio::io::sink()).await });
        (
            Self {
                child,
                stderr: Some(stderr),
            },
            pipes.stdout,
            pipes.stdin,
            directory,
        )
    }
}

impl Drop for NativePeer {
    fn drop(&mut self) {
        // Stderr is reactor-backed and contains untrusted logs. Abort its reader
        // without a blocking join, even if the native stop request fails.
        let _ = match &mut self.child {
            Child::Isolated(child) => child.kill(),
            #[cfg(any(windows, target_os = "linux"))]
            Child::Trusted(child) => child.kill(),
        };
        if let Some(stderr) = self.stderr.take() {
            stderr.abort();
        }
    }
}
