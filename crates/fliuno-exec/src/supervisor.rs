//! Process-tool adapter for the core's durable handoff. The registered program
//! and its resolved environment cross a private stdin pipe only.
use crate::{Program, ProgramConfig};
use fliuno_core::{
    CancellationToken, Error, Result, SupervisorTicket, Tool, ToolContext, ToolError,
    diagnostics::{self, Destinations, Scope},
};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    io::{Read, Write},
    sync::mpsc,
    time::Duration,
};

pub const WORKER_ARG: &str = "--process-supervisor";
const MAX_BOOTSTRAP: usize = 2 * 1024 * 1024;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Bootstrap<T> {
    ticket: T,
    program: ProgramConfig,
    environment: BTreeMap<String, String>,
    executable_digest: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    diagnostics: Option<(Scope, Destinations)>,
}

pub(crate) fn launch(program: &Program, ticket: &SupervisorTicket) -> Result<()> {
    let worker = program
        .supervisor
        .as_ref()
        .ok_or_else(|| Error::Conflict("supervisor is not configured".into()))?;
    let encoded = serde_json::to_vec(&Bootstrap {
        ticket,
        program: program.config.clone(),
        environment: program.environment.clone(),
        executable_digest: program.executable_digest.clone(),
        diagnostics: Scope::current().zip(diagnostics::worker_destinations()),
    })?;
    if encoded.len() > MAX_BOOTSTRAP {
        return Err(Error::Invalid(
            "supervisor bootstrap exceeds its budget".into(),
        ));
    }
    let mut child = fliuno_sandbox::spawn_service(worker, &[WORKER_ARG.into()])?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| Error::Conflict("supervisor pipe is missing".into()))?;
    std::thread::Builder::new()
        .name("fliuno-process-supervisor-reaper".into())
        .spawn(move || {
            let _ = child.wait();
        })?;
    stdin.write_all(&encoded)?;
    Ok(())
}

/// Only used as a private app-server subprocess entry point.
pub fn run<G>(configure_diagnostics: impl FnOnce(&Destinations) -> Option<G>) -> Result<()> {
    let mut encoded = Vec::new();
    std::io::stdin()
        .take((MAX_BOOTSTRAP + 1) as u64)
        .read_to_end(&mut encoded)?;
    if encoded.len() > MAX_BOOTSTRAP {
        return Err(Error::Invalid(
            "supervisor bootstrap exceeds its budget".into(),
        ));
    }
    let Bootstrap {
        ticket,
        program,
        environment,
        executable_digest,
        diagnostics,
    } = serde_json::from_slice::<Bootstrap<SupervisorTicket>>(&encoded)?;
    drop(encoded);
    let program =
        Program::from_parts(program, environment, executable_digest).map_err(Error::Invalid)?;
    let (mut lease, task, workspace, call) =
        ticket.claim(&program.tool_name, &program.authority())?;
    if !lease.renew()? {
        return Ok(());
    }
    let _diagnostics_writer = diagnostics
        .as_ref()
        .and_then(|(_, output)| configure_diagnostics(output));
    let trace_scope = diagnostics.map(|(scope, _)| scope);
    let cancellation = CancellationToken::new();
    let result = std::thread::scope(|scope| {
        let (send, receive) = mpsc::sync_channel(1);
        let command_cancellation = cancellation.clone();
        scope.spawn(move || {
            let context = ToolContext {
                task_id: &task.id,
                session_id: &task.session_id,
                workspace: &workspace,
            };
            let execute = || program.execute(&context, &call, &command_cancellation);
            let result = match trace_scope {
                Some(scope) => scope.in_scope(execute),
                None => execute(),
            };
            let _ = send.send(result);
        });
        loop {
            match receive.recv_timeout(Duration::from_millis(250)) {
                Ok(result) => break result,
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    break Err(ToolError::OutcomeUnknown(
                        "supervisor lost the process result".into(),
                    ));
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if !cancellation.is_cancelled() && !matches!(lease.renew(), Ok(true)) {
                        cancellation.cancel();
                    }
                }
            }
        }
    });
    lease.finish(result)
}
