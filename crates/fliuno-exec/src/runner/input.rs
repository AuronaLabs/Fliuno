//! File-backed async writers may accept bytes into an intermediate buffer.
//! Shutdown must finish that buffer before input is reported as delivered.
use super::evidence::{ProcessFault, ProcessOperation};
use tokio::io::{AsyncWrite, AsyncWriteExt};

pub(super) async fn write_and_close<W: AsyncWrite + Unpin>(
    mut writer: W,
    bytes: &[u8],
) -> Result<(), ProcessFault> {
    // On either failure the owned writer is dropped before this future resolves.
    // Child refusal to read stdin can be a known result, not proof of consumption.
    writer
        .write_all(bytes)
        .await
        .map_err(|error| ProcessFault::new(ProcessOperation::StdinWrite, &error))?;
    writer
        .shutdown()
        .await
        .map_err(|error| ProcessFault::new(ProcessOperation::StdinShutdown, &error))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io,
        pin::Pin,
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
        task::{Context, Poll},
    };

    struct BufferedFailure(Arc<AtomicBool>);
    impl AsyncWrite for BufferedFailure {
        fn poll_write(
            self: Pin<&mut Self>,
            _: &mut Context<'_>,
            data: &[u8],
        ) -> Poll<io::Result<usize>> {
            Poll::Ready(Ok(data.len()))
        }
        fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Err(io::Error::other(
                "private host path must not be recorded",
            )))
        }
        fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            self.poll_flush(cx)
        }
    }
    impl Drop for BufferedFailure {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    #[tokio::test]
    async fn buffered_acceptance_is_not_delivery_and_failed_shutdown_closes_the_writer() {
        let closed = Arc::new(AtomicBool::new(false));
        let fault = write_and_close(BufferedFailure(closed.clone()), b"accepted then lost")
            .await
            .unwrap_err();
        assert_eq!(fault.operation, ProcessOperation::StdinShutdown);
        assert!(closed.load(Ordering::SeqCst));
        assert!(
            !serde_json::to_string(&fault)
                .unwrap()
                .contains("private host path")
        );
    }

    #[cfg(any(unix, windows))]
    #[tokio::test]
    async fn actual_file_backed_pipe_reports_a_flush_error_after_its_reader_exits() {
        use std::{
            fs::File,
            process::{Command, Stdio},
        };
        // The test harness lists its tests and exits without reading stdin.
        // Wait for exit before writing, so successful pipe buffering cannot be
        // mistaken for the child consuming input or for a guaranteed failure.
        let mut child = Command::new(std::env::current_exe().unwrap())
            .arg("--list")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let pipe = child.stdin.take().unwrap();
        #[cfg(windows)]
        let pipe = File::from(std::os::windows::io::OwnedHandle::from(pipe));
        #[cfg(unix)]
        let pipe = File::from(std::os::fd::OwnedFd::from(pipe));
        assert!(child.wait().unwrap().success());
        let fault = write_and_close(
            tokio::fs::File::from_std(pipe),
            b"buffered before OS failure",
        )
        .await
        .unwrap_err();
        assert_eq!(fault.operation, ProcessOperation::StdinShutdown);
        assert_eq!(fault.kind, "BrokenPipe");
    }
}
