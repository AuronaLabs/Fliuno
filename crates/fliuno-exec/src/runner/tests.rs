//! Deterministic scheduling for deadline/stop races which native startup speed
//! cannot reliably reproduce. Real process/pipe probes live in integration tests.
use super::*;
use crate::ProgramConfig;
use fliuno_sandbox::{AsyncStdio, PipeReader, PipeWriter};
use std::{
    pin::Pin,
    process::ExitStatus,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    task::{Context, Poll},
};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

struct ProbeWriter {
    polls: Arc<AtomicUsize>,
    drops: Arc<AtomicUsize>,
    pending: bool,
}
impl AsyncWrite for ProbeWriter {
    fn poll_write(
        self: Pin<&mut Self>,
        _: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        self.polls.fetch_add(1, Ordering::SeqCst);
        if self.pending {
            Poll::Pending
        } else {
            Poll::Ready(Ok(bytes.len()))
        }
    }
    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
    fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}
impl Drop for ProbeWriter {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
    }
}
struct PendingReader;
impl AsyncRead for PendingReader {
    fn poll_read(
        self: Pin<&mut Self>,
        _: &mut Context<'_>,
        _: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Poll::Pending
    }
}
struct NativeObservation {
    pipes: Option<AsyncStdio>,
    stops: Arc<AtomicUsize>,
    natural_exit: bool,
    stop_fails: bool,
}
impl ProcessControl for NativeObservation {
    fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        Ok((self.natural_exit || self.stops.load(Ordering::SeqCst) > 0).then(ExitStatus::default))
    }
    fn stop(&mut self) -> io::Result<()> {
        self.stops.fetch_add(1, Ordering::SeqCst);
        if self.stop_fails {
            Err(io::ErrorKind::PermissionDenied.into())
        } else {
            Ok(())
        }
    }
    fn pipes(&mut self) -> io::Result<(PipeWriter, PipeReader, PipeReader)> {
        let pipes = self.pipes.take().unwrap();
        Ok((pipes.stdin, pipes.stdout, pipes.stderr))
    }
}
fn program() -> Program {
    Program::resolve(ProgramConfig {
        assets: vec![],
        id: "scheduling_probe".into(),
        command: std::env::current_exe().unwrap(),
        isolation: ProcessIsolation::TrustedHost,
        resources: Default::default(),
        linux_cgroup_parent: None,
        args: vec![],
        env_allow: vec![],
        timeout_ms: 30_000,
        output_bytes: 1024,
    })
    .unwrap()
}
fn arguments() -> Arguments {
    Arguments {
        args: vec![],
        stdin: "must not be written after stop".into(),
        timeout_ms: None,
        output_bytes: None,
    }
}
fn observations(
    pending_write: bool,
    pending_output: bool,
) -> (NativeObservation, Arc<AtomicUsize>, Arc<AtomicUsize>) {
    let polls = Arc::new(AtomicUsize::new(0));
    let drops = Arc::new(AtomicUsize::new(0));
    let stdout: PipeReader = if pending_output {
        Box::new(PendingReader)
    } else {
        Box::new(std::io::Cursor::new(b"retained output".to_vec()))
    };
    (
        NativeObservation {
            pipes: Some(AsyncStdio {
                stdin: Box::new(ProbeWriter {
                    polls: polls.clone(),
                    drops: drops.clone(),
                    pending: pending_write,
                }),
                stdout,
                stderr: Box::new(tokio::io::empty()),
            }),
            stops: Arc::new(AtomicUsize::new(0)),
            natural_exit: false,
            stop_fails: false,
        },
        polls,
        drops,
    )
}

#[tokio::test(start_paused = true)]
async fn expired_launch_and_already_cancelled_launch_never_poll_input() {
    let program = program();
    for cancelled in [false, true] {
        let (process, polls, drops) = observations(false, false);
        let stop = CancellationToken::new();
        if cancelled {
            stop.cancel();
        }
        let now = tokio::time::Instant::now();
        let deadline = if cancelled {
            now + Duration::from_secs(30)
        } else {
            now
        };
        let report = drive(&program, arguments(), &stop, process, now, deadline)
            .await
            .unwrap();
        assert_eq!(polls.load(Ordering::SeqCst), 0);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        assert_eq!(
            report.evidence.stop_cause,
            Some(if cancelled {
                ProcessStopCause::Cancelled
            } else {
                ProcessStopCause::Deadline
            })
        );
        assert!(report.evidence.stdin_skipped && report.evidence.stdin_closed);
        assert!(!report.stdin_complete && !report.evidence.stdin_aborted);
        assert!(
            report.evidence.root_reaped
                && report.evidence.stdout_closed
                && report.evidence.stderr_closed
        );
        assert_eq!(report.stdout.data, "retained output");
    }
}

#[tokio::test(start_paused = true)]
async fn cleanup_expiry_retains_cancel_reason_and_does_not_claim_output_eof() {
    let (process, polls, drops) = observations(false, true);
    let stop = CancellationToken::new();
    stop.cancel();
    let now = tokio::time::Instant::now();
    let report = drive(
        &program(),
        arguments(),
        &stop,
        process,
        now,
        now + Duration::from_secs(30),
    )
    .await
    .unwrap();
    assert_eq!(report.termination, Termination::CleanupFailed);
    assert_eq!(
        report.evidence.stop_cause,
        Some(ProcessStopCause::Cancelled)
    );
    assert!(report.evidence.cleanup_expired && report.evidence.root_reaped);
    assert!(!report.evidence.stdout_closed && report.evidence.stderr_closed);
    assert_eq!(polls.load(Ordering::SeqCst), 0);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
}

#[tokio::test(start_paused = true)]
async fn natural_exit_with_stalled_input_reports_abandonment_when_drain_expires() {
    let (mut process, polls, drops) = observations(true, false);
    process.natural_exit = true;
    let now = tokio::time::Instant::now();
    let report = drive(
        &program(),
        arguments(),
        &CancellationToken::new(),
        process,
        now,
        now + Duration::from_secs(30),
    )
    .await
    .unwrap();
    assert_eq!(report.termination, Termination::CleanupFailed);
    assert_eq!(
        report.evidence.stop_cause,
        Some(ProcessStopCause::RootExited)
    );
    assert!(report.evidence.cleanup_expired && report.evidence.root_reaped);
    assert!(polls.load(Ordering::SeqCst) > 0);
    assert!(report.evidence.stdin_aborted && !report.evidence.stdin_skipped);
    assert!(!report.stdin_complete && !report.evidence.stdin_closed);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
}

#[tokio::test(start_paused = true)]
async fn reaped_root_cannot_hide_an_unrelated_termination_failure() {
    let (mut process, _, _) = observations(false, false);
    process.natural_exit = true;
    process.stop_fails = true;
    let now = tokio::time::Instant::now();
    let report = drive(
        &program(),
        arguments(),
        &CancellationToken::new(),
        process,
        now,
        now + Duration::from_secs(30),
    )
    .await
    .unwrap();
    assert_eq!(report.termination, Termination::CleanupFailed);
    assert!(!report.success && report.evidence.root_reaped);
    assert_eq!(
        report.evidence.stop_cause,
        Some(ProcessStopCause::RootExited)
    );
    assert_eq!(report.evidence.stop_request_succeeded, Some(false));
    assert_eq!(
        report.evidence.faults[0].operation,
        ProcessOperation::Terminate
    );
    assert_eq!(report.evidence.faults[0].kind, "PermissionDenied");
}
