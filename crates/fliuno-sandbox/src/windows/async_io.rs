//! IOCP owns all pending pipe operations, including cancellation on Drop.
use crate::{AsyncStdio, PipeReader, PipeWriter};
use std::{
    fs::File,
    io,
    os::windows::io::IntoRawHandle,
    pin::Pin,
    task::{Context, Poll, ready},
};
use tokio::{io::AsyncWrite, net::windows::named_pipe::NamedPipeServer};

pub(crate) fn register(input: File, output: File, error: File) -> io::Result<AsyncStdio> {
    fn pipe(file: File) -> io::Result<NamedPipeServer> {
        // SAFETY: only the native async launchers supply these owned server
        // handles, created with FILE_FLAG_OVERLAPPED. Tokio consumes ownership
        // even on registration failure; no other owner retains the handle.
        unsafe { NamedPipeServer::from_raw_handle(file.into_raw_handle()) }
    }
    let stdin: PipeWriter = Box::new(Writer {
        pipe: Some(pipe(input)?),
        dirty: false,
    });
    let stdout: PipeReader = Box::new(pipe(output)?);
    let stderr: PipeReader = Box::new(pipe(error)?);
    Ok(AsyncStdio {
        stdin,
        stdout,
        stderr,
    })
}

struct Writer {
    pipe: Option<NamedPipeServer>,
    dirty: bool,
}
impl Writer {
    fn open(&self) -> io::Result<&NamedPipeServer> {
        self.pipe
            .as_ref()
            .ok_or_else(|| io::ErrorKind::BrokenPipe.into())
    }
}
impl AsyncWrite for Writer {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        if bytes.is_empty() {
            self.open()?;
            return Poll::Ready(Ok(0));
        }
        let pipe = self.pipe.as_mut().ok_or(io::ErrorKind::BrokenPipe)?;
        // Bound the buffer copied into mio's single in-flight WriteFile request.
        let result = ready!(Pin::new(pipe).poll_write(cx, &bytes[..bytes.len().min(64 * 1024)]));
        if result.as_ref().is_ok_and(|count| *count > 0) {
            self.dirty = true;
        }
        Poll::Ready(result)
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let pipe = self.open()?;
        if !self.dirty {
            return Poll::Ready(Ok(()));
        }
        // mio accepts a write when it submits overlapped IO. Tokio's default
        // NamedPipeServer::poll_flush is a no-op, so wait until that operation
        // has completed and surface its error via the next zero-byte write.
        // try_write clears stale readiness on WouldBlock. The zero-byte probe
        // adds no data and does not wait for the peer to consume earlier bytes.
        loop {
            ready!(pipe.poll_write_ready(cx))?;
            match pipe.try_write(&[]) {
                Ok(_) => {
                    self.dirty = false;
                    return Poll::Ready(Ok(()));
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => continue,
                Err(error) => return Poll::Ready(Err(error)),
            }
        }
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        if self.pipe.is_none() {
            return Poll::Ready(Ok(()));
        }
        ready!(self.as_mut().poll_flush(cx))?;
        // Closing the parent end delivers EOF to the child after the last
        // completed data write. It also cancels the harmless zero-byte probe.
        self.pipe.take();
        Poll::Ready(Ok(()))
    }
}
