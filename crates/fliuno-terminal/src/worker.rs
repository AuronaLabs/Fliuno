//! Dedicated process bootstrap and bounded IPC accept loop. PTY IO, command
//! state transitions and storage are separate modules with one owner each.
mod actor;
pub(crate) mod pty;
use crate::{config, ipc, protocol::*, spool::Spool};
use interprocess::local_socket::{
    ListenerOptions,
    tokio::{Listener, prelude::*},
};
use serde_json::Value;
use std::{
    fs::OpenOptions,
    io::{Read, Write},
    time::Duration,
};
use tokio::sync::{mpsc, oneshot};
pub(crate) struct Command {
    pub session_id: String,
    pub operation: Operation,
    pub reply: oneshot::Sender<Result<Value, String>>,
}
/// Called only for the internal --terminal-worker command. Bootstrap is bounded
/// and read before any PTY launch; it is never interpreted as public JSON-RPC.
pub fn run() -> Result<(), String> {
    let mut bytes = Vec::new();
    std::io::stdin()
        .take(FRAME_LIMIT as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "cannot read terminal bootstrap")?;
    if bytes.len() > FRAME_LIMIT {
        return Err("terminal bootstrap exceeds limit".into());
    }
    let launch: Launch =
        serde_json::from_slice(&bytes).map_err(|_| "invalid terminal bootstrap")?;
    if !launch.directory.is_absolute()
        || !launch.cwd.is_absolute()
        || !launch.cwd.is_dir()
        || launch.descriptor.version != 1
        || launch.descriptor.token.len() != 64
        || launch.snapshot.terminal_id.len() != 64
        || !(65536..=8 * 1024 * 1024).contains(&launch.spool_bytes)
        || !(1000..=86_400_000).contains(&launch.lifetime_ms)
    {
        return Err("invalid terminal bootstrap authority or budgets".into());
    }
    fliuno_sandbox::LaunchSpec {
        program_sha256: None,
        assets: vec![],
        program: launch.command.clone(),
        args: launch.args.clone(),
        env: launch.environment.clone(),
        resources: launch.resources,
        linux_cgroup_parent: launch.linux_cgroup_parent.clone(),
    }
    .validate()
    .map_err(|_| "invalid terminal bootstrap program")?;
    #[cfg(windows)]
    fliuno_sandbox::contain_service_process(launch.resources)
        .map_err(|_| "cannot contain terminal worker")?;
    let owner = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(launch.directory.join("owner.lock"))
        .map_err(|_| "cannot open terminal ownership lock")?;
    owner.try_lock().map_err(|_| "terminal already owned")?;
    let root = launch
        .directory
        .parent()
        .ok_or("invalid terminal state root")?;
    if crate::registry::Registry::open(root, false)?
        .get(&launch.snapshot.terminal_id)?
        .is_some()
    {
        return Err("terminal identity was archived; worker must never launch it".into());
    }
    // The dedicated process, not a stack frame, owns this lock until OS exit.
    std::mem::forget(owner);
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .map_err(|_| "cannot create terminal worker runtime")?;
    // A writer may be blocked in the native PTY. Returning from run must not
    // wait forever for a Tokio blocking pool; native reader/writer threads are
    // intentionally process-owned and released by OS exit, not joined here.
    let result = runtime.block_on(serve(launch));
    runtime.shutdown_background();
    result
}
async fn serve(mut launch: Launch) -> Result<(), String> {
    launch.descriptor.supervisor_pid = std::process::id();
    let spool = Spool::open_reserved(&launch.directory, launch.spool_bytes, &launch.snapshot)?;
    let listener = prepare_endpoint(&launch).inspect_err(|message| {
        let _ = spool.startup_failed(message, false);
    })?;
    // Once native PTY startup is attempted, a platform error can happen after
    // child creation. Keep that outcome uncertain rather than promising no IO.
    let (pty, mut events) = pty::spawn(pty::Process {
        command: &launch.command,
        args: &launch.args,
        environment: &launch.environment,
        cwd: &launch.cwd,
        dimensions: (launch.snapshot.rows, launch.snapshot.cols),
    })
    .inspect_err(|message| {
        let _ = spool.startup_failed(message, true);
    })?;
    let mut actor = actor::Actor::new(spool, pty, launch.lifetime_ms)?;
    let mut previous = actor.snapshot()?;
    let (changes, _) = tokio::sync::watch::channel(());
    let subscribers = std::sync::Arc::new(tokio::sync::Semaphore::new(8));
    let (tx, mut requests) = mpsc::channel::<Command>(16);
    let capacity = std::sync::Arc::new(tokio::sync::Semaphore::new(16));
    let mut tick = tokio::time::interval(Duration::from_millis(20));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            connection=listener.accept()=>{
                if let Ok(stream)=connection && let Ok(permit)=capacity.clone().try_acquire_owned(){
                    let token=launch.descriptor.token.clone();let tx=tx.clone();
                    let changes=changes.subscribe();let subscribers=subscribers.clone();
                    tokio::spawn(async move{let _permit=permit;ipc::serve_connection(stream,&token,tx,changes,subscribers).await;});
                }
            },
            Some(command)=requests.recv()=>actor.command(command)?,
            Some(event)=events.recv()=>actor.event(event)?,
            _=tick.tick()=>if actor.tick()?{break;}
        }
        // Publish only committed state changes. In particular, reading output
        // must not wake another read and create a self-sustaining busy loop.
        let snapshot = actor.snapshot()?;
        if previous != snapshot {
            previous = snapshot;
            changes.send_replace(());
        }
    }
    Ok(())
}

/// Everything here precedes native PTY launch and can be reported as a known
/// startup refusal. Messages are authored by us, never interpolated stderr.
fn prepare_endpoint(launch: &Launch) -> Result<Listener, String> {
    if config::digest(&launch.command)? != launch.executable_digest {
        return Err("terminal executable changed after approval".into());
    }
    dimensions(launch.snapshot.rows, launch.snapshot.cols)?;
    let listener = ListenerOptions::new()
        .name(ipc::name(&launch.descriptor.endpoint).map_err(|_| "invalid terminal endpoint")?)
        .create_tokio()
        .map_err(|_| "cannot bind terminal endpoint")?;
    let endpoint =
        serde_json::to_vec(&launch.descriptor).map_err(|_| "cannot encode terminal endpoint")?;
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(launch.directory.join("endpoint.json"))
        .map_err(|_| "terminal endpoint already exists")?;
    file.write_all(&endpoint)
        .and_then(|_| file.sync_all())
        .map_err(|_| "cannot save terminal endpoint")?;
    Ok(listener)
}
