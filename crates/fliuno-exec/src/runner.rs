//! Own one synchronous command lifetime while draining both pipes concurrently.
//! Even a successful root exit closes its descendants: detached background jobs
//! require a separate durable process service rather than leaking from a tool.
mod evidence;
mod input;
mod process;
use crate::{Arguments, CapturedOutput, Program, capture::Capture, config::ProcessIsolation};
pub use evidence::{ProcessEvidence, ProcessFault, ProcessOperation, ProcessStopCause};
use fliuno_core::{CancellationToken, ToolContext, ToolError};
use process::{Process, ProcessControl};
use serde::Serialize;
use std::{io, time::Duration};
use tokio::io::AsyncReadExt;

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Termination {
    Exited,
    TimedOut,
    Cancelled,
    IoError,
    CleanupFailed,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProcessReport {
    pub program_id: String,
    pub isolation: ProcessIsolation,
    pub termination: Termination,
    pub exit_code: Option<i32>,
    pub success: bool,
    pub stdin_complete: bool,
    pub elapsed_ms: u64,
    pub stdout: CapturedOutput,
    pub stderr: CapturedOutput,
    pub evidence: ProcessEvidence,
}

pub(crate) async fn run(
    program: &Program,
    context: &ToolContext<'_>,
    args: Arguments,
    cancellation: &CancellationToken,
) -> Result<ProcessReport, ToolError> {
    let started = tokio::time::Instant::now();
    let timeout = args.timeout_ms.unwrap_or(program.config.timeout_ms);
    let deadline = started + Duration::from_millis(timeout);
    let process = Process::spawn(program, context, &args).await?;
    drive(program, args, cancellation, process, started, deadline).await
}

async fn drive(
    program: &Program,
    args: Arguments,
    cancellation: &CancellationToken,
    mut process: impl ProcessControl,
    started: tokio::time::Instant,
    deadline: tokio::time::Instant,
) -> Result<ProcessReport, ToolError> {
    let mut evidence = ProcessEvidence {
        launch_ms: started.elapsed().as_millis().min(u64::MAX as u128) as u64,
        ..ProcessEvidence::default()
    };
    let (stdin, mut stdout, mut stderr) = process
        .pipes()
        .map_err(|_| ToolError::OutcomeUnknown("cannot open launched process pipes".into()))?;
    let mut stdin_complete = false;
    let mut input = Some(Box::pin(input::write_and_close(
        stdin,
        args.stdin.as_bytes(),
    )));
    let mut input_polled = false;
    let mut stdout_done = false;
    let mut stderr_done = false;
    let mut output = Capture::new(args.output_bytes.unwrap_or(program.config.output_bytes));
    let mut errors = Capture::new(args.output_bytes.unwrap_or(program.config.output_bytes));
    let mut out_buffer = [0; 8192];
    let mut err_buffer = [0; 8192];
    let mut status = None;
    let mut termination = Termination::Exited;
    let mut stopping = false;
    let mut cleanup_deadline = deadline;
    let mut tick = tokio::time::interval(Duration::from_millis(10));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        // Native launch is synchronous and consumes the same total budget.
        // Check already-observed stop conditions before polling any input IO;
        // a select between a ready writer and an expired deadline is a race.
        if !stopping {
            if termination == Termination::Exited && status.is_none() {
                if cancellation.is_cancelled() {
                    termination = Termination::Cancelled;
                } else if tokio::time::Instant::now() >= deadline {
                    termination = Termination::TimedOut;
                }
            }
            if termination != Termination::Exited || status.is_some() {
                let cause = match termination {
                    Termination::Exited => ProcessStopCause::RootExited,
                    Termination::TimedOut => ProcessStopCause::Deadline,
                    Termination::Cancelled => ProcessStopCause::Cancelled,
                    _ => ProcessStopCause::IoError,
                };
                evidence.stop_cause = Some(cause);
                if cause != ProcessStopCause::RootExited {
                    // Do not queue another write after observing cancellation.
                    // Only an unpolled writer can prove no background IO owns
                    // the pipe. An in-flight write remains conservative evidence.
                    abandon_input(&mut input, input_polled, &mut evidence);
                }
                let killed = process.stop();
                evidence.stop_request_succeeded = Some(killed.is_ok());
                if let Err(error) = &killed {
                    evidence.record(ProcessFault::new(ProcessOperation::Terminate, error));
                }
                // Only an absent Unix host group after root reap is benign;
                // root exit never hides an unrelated Job/cgroup cleanup failure.
                if killed
                    .as_ref()
                    .is_err_and(|error| !process.stop_found_absent_group(status.is_some(), error))
                {
                    termination = Termination::CleanupFailed;
                }
                stopping = true;
                cleanup_deadline = tokio::time::Instant::now() + Duration::from_secs(2);
            }
        }
        if status.is_some() && input.is_none() && stdout_done && stderr_done {
            break;
        }
        tokio::select! {
            bytes=stdout.read(&mut out_buffer),if !stdout_done=>match bytes {
                Ok(0)=>{stdout_done=true;evidence.stdout_closed=true;},
                Ok(count)=>output.push(&out_buffer[..count]),
                Err(error) if error.kind()==io::ErrorKind::BrokenPipe=>{stdout_done=true;evidence.stdout_closed=true;},
                Err(error)=>{stdout_done=true;termination=Termination::IoError;evidence.record(ProcessFault::new(ProcessOperation::StdoutRead,&error));},
            },
            bytes=stderr.read(&mut err_buffer),if !stderr_done=>match bytes {
                Ok(0)=>{stderr_done=true;evidence.stderr_closed=true;},
                Ok(count)=>errors.push(&err_buffer[..count]),
                Err(error) if error.kind()==io::ErrorKind::BrokenPipe=>{stderr_done=true;evidence.stderr_closed=true;},
                Err(error)=>{stderr_done=true;termination=Termination::IoError;evidence.record(ProcessFault::new(ProcessOperation::StderrRead,&error));},
            },
            complete=std::future::poll_fn(|cx| {
                input_polled=true;
                std::future::Future::poll(input.as_mut().unwrap().as_mut(),cx)
            }),if input.is_some()=>{
                input=None;evidence.stdin_closed=true;stdin_complete=complete.is_ok();
                if let Err(fault)=complete { evidence.record(fault); }
            },
            _=cancellation.cancelled(),if !stopping=>{termination=Termination::Cancelled;},
            _=tokio::time::sleep_until(deadline),if !stopping=>{termination=Termination::TimedOut;},
            _=tokio::time::sleep_until(cleanup_deadline),if stopping=>{
                evidence.cleanup_expired=true;
                abandon_input(&mut input, input_polled, &mut evidence);
                termination=Termination::CleanupFailed;
                break;
            },
            _=tick.tick()=>{
                if status.is_none(){match process.try_wait(){Ok(value)=>status=value,Err(error)=>{termination=Termination::IoError;evidence.record(ProcessFault::new(ProcessOperation::PollExit,&error));}}}
            },
        }
    }
    evidence.root_reaped = status.is_some();
    Ok(ProcessReport {
        program_id: program.config.id.clone(),
        isolation: program.config.isolation,
        termination,
        exit_code: status.and_then(|status| status.code()),
        success: termination == Termination::Exited
            && status.is_some_and(|status| status.success())
            && stdin_complete,
        stdin_complete,
        elapsed_ms: started.elapsed().as_millis().min(u64::MAX as u128) as u64,
        stdout: output.finish(),
        stderr: errors.finish(),
        evidence,
    })
}

fn abandon_input<T>(input: &mut Option<T>, polled: bool, evidence: &mut ProcessEvidence) {
    if input.take().is_some() {
        evidence.stdin_skipped = !polled;
        evidence.stdin_aborted = polled;
        evidence.stdin_closed = !polled;
    }
}

#[cfg(test)]
mod tests;
