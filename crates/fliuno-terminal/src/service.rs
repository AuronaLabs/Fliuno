//! Host-side service facade. Workers are independent processes; dropping this
//! value or app-server does not close terminals. IDs are deterministic from
//! durable tool-call identity, and existing IDs are never relaunched.
use crate::{
    archive,
    config::Preset,
    ipc,
    protocol::*,
    registry::{self, Registry},
    spool::{self, OutputCursor},
};
use fliuno_core::{CancellationToken, ToolContext, ToolError};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{fs, io::Write, path::PathBuf, sync::Arc, time::Duration};
const OBSERVATION_TIMEOUT: Duration = Duration::from_millis(150);
pub(crate) const REGISTRY_SCAN_LIMIT: usize = 128;
const MAX_RECORDS: usize = 64;

#[derive(Clone)]
pub struct Service {
    pub(crate) root: Arc<PathBuf>,
    worker: Arc<PathBuf>,
    pub(crate) retention_grace_ms: Option<u64>,
}
struct StagedDirectory {
    path: PathBuf,
    published: bool,
}
impl StagedDirectory {
    fn create(root: &std::path::Path) -> Result<Self, String> {
        let path = root.join(format!(".staging-{}", uuid::Uuid::new_v4().simple()));
        fliuno_sandbox::create_private_directory(&path)
            .map_err(|_| "cannot create private terminal staging directory")?;
        Ok(Self {
            path,
            published: false,
        })
    }
    fn publish(&mut self, destination: &std::path::Path) -> Result<(), String> {
        // Both paths are siblings on one filesystem. The deterministic ID
        // becomes visible only after its complete initial spool exists.
        fs::rename(&self.path, destination).map_err(|_| "cannot publish terminal reservation")?;
        self.published = true;
        Ok(())
    }
}
impl Drop for StagedDirectory {
    fn drop(&mut self) {
        if !self.published {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}
pub(crate) struct CreateOptions {
    pub args: Vec<String>,
    pub rows: u16,
    pub cols: u16,
}
impl Service {
    pub(crate) fn new(root: PathBuf, worker: PathBuf) -> Result<Self, String> {
        if !root.is_absolute() || !worker.is_absolute() || !worker.is_file() {
            return Err("terminal stateDirectory and worker must be absolute paths".into());
        }
        if !root.exists() {
            fliuno_sandbox::create_private_directory(&root)
                .map_err(|_| "cannot create terminal state directory; parent must exist")?;
        }
        let metadata =
            fs::symlink_metadata(&root).map_err(|_| "cannot inspect terminal state directory")?;
        if !metadata.is_dir() || registry::reparse(&metadata) {
            return Err("terminal state directory must be a real directory".into());
        }
        let root = fs::canonicalize(root).map_err(|_| "cannot resolve terminal state directory")?;
        let _lock = registry::lock(&root)?;
        let ledger = Registry::initialize(&root)?;
        archive::recover(&root, &ledger)?;
        Ok(Self {
            root: Arc::new(root),
            worker: Arc::new(worker),
            retention_grace_ms: None,
        })
    }

    pub(crate) fn apply_retention(&self) -> Result<(), ToolError> {
        if let Some(grace_ms) = self.retention_grace_ms {
            let _lock = registry::lock(&self.root)?;
            let mut ledger = Registry::open(&self.root, true)?;
            archive::retention::sweep(self, &mut ledger, now_ms(), grace_ms)?;
        }
        Ok(())
    }
    pub fn directory(&self, id: &str) -> Result<PathBuf, String> {
        registry::valid_id(id)?;
        let path = self.root.join(id);
        let metadata = fs::symlink_metadata(&path).map_err(|_| "terminal not found")?;
        if !metadata.is_dir() || registry::reparse(&metadata) {
            return Err("invalid terminal directory".into());
        }
        Ok(path)
    }
    /// Trusted-host API. Mutations require the caller's durable approval and
    /// operation ID. Model-facing code must use the registered Tool adapters.
    pub async fn call(
        &self,
        session: &str,
        id: &str,
        operation: Operation,
    ) -> Result<Value, String> {
        // Check persisted ownership before exposing a capability to any caller.
        let saved = self.offline(session, id, 0, 1)?;
        if saved.snapshot.archived_at_unix_ms.is_some() {
            return Err(
                "terminal identity is archived; it cannot be controlled or relaunched".into(),
            );
        }
        let dir = self.directory(id)?;
        let descriptor = ipc::descriptor(&dir)?;
        ipc::call(&descriptor, session, operation).await
    }
    pub async fn status(&self, session: &str, id: &str) -> Result<Snapshot, String> {
        match tokio::time::timeout(
            OBSERVATION_TIMEOUT,
            self.call(session, id, Operation::Status),
        )
        .await
        {
            Ok(Ok(value)) => {
                serde_json::from_value(value).map_err(|_| "invalid terminal status".into())
            }
            Err(_) => Ok(self.offline(session, id, 0, 1)?.snapshot),
            Ok(Err(_)) => Ok(self.offline(session, id, 0, 1)?.snapshot),
        }
    }
    pub async fn output(
        &self,
        session: &str,
        id: &str,
        after: u64,
        limit: usize,
    ) -> Result<Output, String> {
        if !(1..=READ_LIMIT).contains(&limit) || after > i64::MAX as u64 {
            return Err("invalid terminal output cursor or limit".into());
        }
        match tokio::time::timeout(
            OBSERVATION_TIMEOUT,
            self.call(session, id, Operation::Output { after, limit }),
        )
        .await
        {
            Ok(Ok(value)) => {
                serde_json::from_value(value).map_err(|_| "invalid terminal output".into())
            }
            Ok(Err(_)) | Err(_) => self.offline(session, id, after, limit),
        }
    }
    /// Read the latest committed bytes and their resume cursor from one snapshot.
    pub fn tail(&self, session: &str, id: &str, limit: usize) -> Result<Output, String> {
        if !(1..=READ_LIMIT).contains(&limit) {
            return Err("invalid terminal output limit".into());
        }
        self.saved_output(session, id, OutputCursor::Tail, limit)
    }

    pub(crate) fn offline(
        &self,
        session: &str,
        id: &str,
        after: u64,
        limit: usize,
    ) -> Result<Output, String> {
        self.saved_output(session, id, OutputCursor::After(after), limit)
    }

    fn saved_output(
        &self,
        session: &str,
        id: &str,
        cursor: OutputCursor,
        limit: usize,
    ) -> Result<Output, String> {
        let ledger = Registry::open(&self.root, false)?;
        let archived = |record: fliuno_protocol::TerminalArchive| {
            if record.snapshot.session_id != session {
                return Err("terminal belongs to another session".into());
            }
            let after = match cursor {
                OutputCursor::After(after) => after,
                OutputCursor::Tail => 0,
            };
            registry::output(record.snapshot, after, limit)
        };
        if let Some(record) = ledger.get(id)? {
            return archived(record);
        }
        let output = match self
            .directory(id)
            .and_then(|dir| spool::read_saved_output(&dir, cursor, limit))
        {
            Ok(output) => output,
            Err(error) => {
                return match ledger.get(id)? {
                    Some(record) => archived(record),
                    None => Err(error),
                };
            }
        };
        if output.snapshot.session_id != session {
            return Err("terminal belongs to another session".into());
        }
        Ok(output)
    }
    /// Host-configured roots hold at most 64 records. No unbounded directory
    /// scan or supervisor startup is triggered by a client listing terminals.
    pub async fn list(&self, session: &str) -> Result<Vec<Snapshot>, String> {
        let mut snapshots = Vec::new();
        let refresh_deadline = tokio::time::Instant::now() + OBSERVATION_TIMEOUT;
        for entry in fs::read_dir(&*self.root)
            .map_err(|_| "cannot list terminals")?
            .take(REGISTRY_SCAN_LIMIT)
        {
            let entry = entry.map_err(|_| "cannot list terminals")?;
            let id = entry.file_name().to_string_lossy().into_owned();
            if id.len() != 64 {
                continue;
            }
            if let Ok(output) = self.offline(session, &id, 0, 1) {
                let mut snapshot = output.snapshot;
                if snapshot.archived_at_unix_ms.is_some() {
                    continue;
                }
                if snapshot.status == Status::OutcomeUnknown {
                    let remaining =
                        refresh_deadline.saturating_duration_since(tokio::time::Instant::now());
                    if remaining.is_zero() {
                        snapshots.push(snapshot);
                        continue;
                    }
                    if let Ok(Ok(value)) =
                        tokio::time::timeout(remaining, self.call(session, &id, Operation::Status))
                            .await
                        && let Ok(live) = serde_json::from_value(value)
                    {
                        snapshot = live;
                    }
                }
                snapshots.push(snapshot);
            }
        }
        snapshots.sort_by(|a, b| a.terminal_id.cmp(&b.terminal_id));
        Ok(snapshots)
    }
    fn capacity(&self, ledger: &Registry) -> Result<(usize, usize), String> {
        let mut count = 0;
        let mut live = 0;
        for (index, entry) in fs::read_dir(&*self.root)
            .map_err(|_| "cannot inspect terminal capacity")?
            .enumerate()
        {
            if index == REGISTRY_SCAN_LIMIT {
                return Err("terminal capacity scan limit exceeded".into());
            }
            let entry = entry.map_err(|_| "cannot inspect terminal capacity")?;
            let name = entry.file_name();
            let Some(id) = name.to_str().filter(|id| registry::valid_id(id).is_ok()) else {
                continue;
            };
            count += 1;
            if ledger.get(id)?.is_some() {
                continue;
            }
            // Missing or unreadable reservations still occupy an uncertain slot.
            match self
                .directory(id)
                .and_then(|dir| spool::saved_snapshot(&dir))
            {
                Ok(snapshot)
                    if snapshot.terminal_id == id
                        && snapshot.archived_at_unix_ms.is_none()
                        && snapshot.status != Status::OutcomeUnknown => {}
                _ => live += 1,
            }
        }
        Ok((count, live))
    }

    pub(crate) async fn create(
        &self,
        preset: &Preset,
        context: &ToolContext<'_>,
        call_id: &str,
        options: CreateOptions,
        cancellation: &CancellationToken,
    ) -> Result<Value, ToolError> {
        let CreateOptions { args, rows, cols } = options;
        let terminal_id = identity(&[&preset.identity, context.task_id, call_id]);
        let directory = self.root.join(&terminal_id);
        // Serialize capacity reservation across independent app-server hosts.
        // std File locks are released on crash; no stale-lock-file inference.
        let lock = registry::lock(&self.root)?;
        let mut ledger = Registry::open(&self.root, true)?;
        if ledger.get(&terminal_id)?.is_some() {
            return Err(uncertain_start(
                context.session_id,
                &terminal_id,
                "terminal identity was archived; never relaunch it",
            ));
        }
        archive::recover(&self.root, &ledger)?;
        cleanup_staging(&self.root)?;
        match fs::symlink_metadata(&directory) {
            Ok(_) => {
                return Err(uncertain_start(
                    context.session_id,
                    &terminal_id,
                    "terminal id was already reserved; never relaunch it",
                ));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Err("cannot inspect terminal identity reservation".into()),
        }
        let (mut count, mut live) = self.capacity(&ledger)?;
        if count >= MAX_RECORDS
            && live < 8
            && let Some(grace_ms) = self.retention_grace_ms
        {
            archive::retention::sweep(self, &mut ledger, now_ms(), grace_ms)?;
            (count, live) = self.capacity(&ledger)?;
        }
        if count >= MAX_RECORDS || live >= 8 {
            return Err(
                "terminal registry capacity reached (64 records / 8 active or uncertain)".into(),
            );
        }
        if cancellation.is_cancelled() {
            return Err("terminal cancelled before creation".into());
        }
        if crate::config::digest(&preset.config.command)? != preset.executable_digest {
            return Err("terminal executable changed; restart and obtain new approval".into());
        }
        let descriptor = Descriptor {
            version: 1,
            supervisor_pid: 0,
            endpoint: ipc::endpoint(&directory)?,
            token: format!(
                "{}{}",
                uuid::Uuid::new_v4().simple(),
                uuid::Uuid::new_v4().simple()
            ),
        };
        let launch = Launch {
            directory: directory.clone(),
            descriptor: descriptor.clone(),
            command: preset.config.command.clone(),
            args: preset.config.args.iter().chain(&args).cloned().collect(),
            environment: preset.environment.clone(),
            executable_digest: preset.executable_digest.clone(),
            resources: preset.config.resources,
            linux_cgroup_parent: preset.config.linux_cgroup_parent.clone(),
            cwd: PathBuf::from(&context.workspace.root),
            spool_bytes: preset.config.spool_bytes,
            lifetime_ms: preset.config.lifetime_ms,
            snapshot: Snapshot {
                terminal_id: terminal_id.clone(),
                session_id: context.session_id.into(),
                preset_id: preset.config.id.clone(),
                status: Status::Starting,
                startup_error: None,
                supervisor_connected: false,
                rows,
                cols,
                exit_code: None,
                signal: None,
                start_offset: 0,
                end_offset: 0,
                input_bytes: 0,
                output_closed: false,
                expires_at_unix_ms: now_ms() + preset.config.lifetime_ms,
                archived_at_unix_ms: None,
            },
        };
        let encoded =
            serde_json::to_vec(&launch).map_err(|_| "cannot encode terminal bootstrap")?;
        if encoded.len() > FRAME_LIMIT {
            return Err("terminal bootstrap exceeds IPC budget".into());
        }
        let mut staging = StagedDirectory::create(&self.root)?;
        let reservation =
            spool::Spool::create(&staging.path, launch.spool_bytes, &launch.snapshot)?;
        drop(reservation);
        staging.publish(&directory)?;
        let reservation =
            spool::Spool::open_reserved(&directory, launch.spool_bytes, &launch.snapshot)?;
        let spawned = fliuno_sandbox::spawn_service_with_limits(
            &self.worker,
            &["--terminal-worker".into()],
            preset.config.resources,
            preset.config.linux_cgroup_parent.as_deref(),
        );
        let mut child = match spawned {
            Ok(child) => child,
            Err(_) => {
                let message = "cannot launch terminal supervisor; no PTY child started";
                let _ = reservation.startup_failed(message, false);
                return Err(message.into());
            }
        };
        drop(reservation);
        // From here the worker may have received bootstrap data. Preserve the
        // terminal identity as structured audit evidence even on cancellation.
        let uncertain = |message: &str| uncertain_start(context.session_id, &terminal_id, message);
        let stdin = child.stdin.take();
        let exited = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let worker_exited = exited.clone();
        std::thread::Builder::new()
            .name("terminal-reaper".into())
            .spawn(move || {
                if let Err(error) = child.wait() {
                    eprintln!("fliuno: terminal supervisor cleanup failed: {error}");
                }
                drop(child);
                worker_exited.store(true, std::sync::atomic::Ordering::Release);
            })
            .map_err(|_| uncertain("cannot start terminal supervisor reaper"))?;
        if let Some(mut stdin) = stdin {
            stdin
                .write_all(&encoded)
                .map_err(|_| uncertain("terminal bootstrap delivery unknown"))?;
        }
        drop(lock);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(8);
        loop {
            if cancellation.is_cancelled() {
                let _ = ipc::call(
                    &descriptor,
                    context.session_id,
                    Operation::Kill {
                        operation_id: identity(&[context.task_id, call_id, "cancel"]),
                    },
                )
                .await;
                return Err(uncertain(
                    "terminal creation cancelled; inspect its durable record",
                ));
            }
            if let Ok(value) = ipc::call(&descriptor, context.session_id, Operation::Status).await {
                if value["status"] == "failed" || value["status"] == "outcome_unknown" {
                    return Err(uncertain(
                        "terminal startup failed; inspect its durable record",
                    ));
                }
                if value["status"] != "starting" {
                    return Ok(value);
                }
            }
            if exited.load(std::sync::atomic::Ordering::Acquire) {
                // Worker startup diagnostics contain only our fixed host
                // messages, never raw stderr or executable arguments.
                if let Ok(saved) = spool::offline(&launch.directory, 0, 1)
                    && saved.snapshot.status == Status::Failed
                {
                    return Err(saved
                        .snapshot
                        .startup_error
                        .unwrap_or_else(|| "terminal startup refused before child launch".into())
                        .into());
                }
                return Err(uncertain(
                    "terminal supervisor stopped during creation; inspect its durable record",
                ));
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(uncertain(
                    "terminal startup outcome unknown; never relaunch automatically",
                ));
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }
}
/// Staging directories are never handed to a worker. The registry lock proves
/// no creator currently owns one, so only abandoned, exact-name children can
/// be removed without touching terminal IDs or worker-owned state.
fn cleanup_staging(root: &std::path::Path) -> Result<(), String> {
    for entry in fs::read_dir(root).map_err(|_| "cannot inspect terminal staging directories")? {
        let entry = entry.map_err(|_| "cannot inspect terminal staging directories")?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let Some(suffix) = name.strip_prefix(".staging-") else {
            continue;
        };
        if suffix.len() != 32 || !suffix.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            continue;
        }
        let metadata = fs::symlink_metadata(entry.path())
            .map_err(|_| "cannot inspect terminal staging directory")?;
        if metadata.is_dir() && !metadata.file_type().is_symlink() {
            fs::remove_dir_all(entry.path())
                .map_err(|_| "cannot remove abandoned terminal staging directory")?;
        }
    }
    Ok(())
}
fn uncertain_start(session: &str, terminal: &str, message: &str) -> ToolError {
    ToolError::OutcomeUnknownWithOutput {
        message: message.into(),
        output: serde_json::json!({"terminalId":terminal,"sessionId":session,"phase":"startup"}),
    }
}
pub(crate) fn identity(parts: &[&str]) -> String {
    format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(parts).expect("string array encodes"))
    )
}
pub(crate) fn runtime() -> Result<tokio::runtime::Runtime, String> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|_| "cannot initialize terminal client runtime".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        ipc,
        protocol::{Descriptor, Snapshot, Status},
        spool::Spool,
    };
    use interprocess::local_socket::{ListenerOptions, tokio::prelude::*};
    use std::{fs::OpenOptions, io::Write, time::Instant};

    #[test]
    fn staging_cleanup_removes_only_private_uuid_named_directories() {
        let root =
            std::env::temp_dir().join(format!("fliuno-terminal-cleanup-{}", uuid::Uuid::new_v4()));
        fliuno_sandbox::create_private_directory(&root).unwrap();
        let abandoned = root.join(format!(".staging-{}", uuid::Uuid::new_v4().simple()));
        fliuno_sandbox::create_private_directory(&abandoned).unwrap();
        fs::write(abandoned.join("output.db"), b"partial").unwrap();
        let lookalike = root.join(".staging-not-an-id");
        fliuno_sandbox::create_private_directory(&lookalike).unwrap();
        cleanup_staging(&root).unwrap();
        assert!(!abandoned.exists());
        assert!(lookalike.is_dir());
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn stalled_live_supervisor_falls_back_to_durable_snapshot_within_budget() {
        let root =
            std::env::temp_dir().join(format!("fliuno-terminal-stalled-{}", uuid::Uuid::new_v4()));
        fliuno_sandbox::create_private_directory(&root).unwrap();
        let id = "a".repeat(64);
        let directory = root.join(&id);
        fliuno_sandbox::create_private_directory(&directory).unwrap();
        let endpoint = ipc::endpoint(&directory).unwrap();
        let listener = ListenerOptions::new()
            .name(ipc::name(&endpoint).unwrap())
            .create_tokio()
            .unwrap();
        let snapshot = Snapshot {
            terminal_id: id.clone(),
            session_id: "session".into(),
            preset_id: "shell".into(),
            status: Status::Running,
            startup_error: None,
            supervisor_connected: false,
            rows: 24,
            cols: 80,
            exit_code: None,
            signal: None,
            start_offset: 0,
            end_offset: 0,
            input_bytes: 0,
            output_closed: false,
            expires_at_unix_ms: 1000,
            archived_at_unix_ms: None,
        };
        let mut spool = Spool::create(&directory, 65536, &snapshot).unwrap();
        spool.append(b"durable output").unwrap();
        drop(spool);
        let descriptor = Descriptor {
            version: 1,
            endpoint,
            token: "b".repeat(64),
            supervisor_pid: 123,
        };
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(directory.join("endpoint.json"))
            .unwrap();
        file.write_all(&serde_json::to_vec(&descriptor).unwrap())
            .unwrap();
        let server = tokio::spawn(async move {
            let _connection = listener.accept().await.unwrap();
            tokio::time::sleep(Duration::from_secs(1)).await;
        });
        let service = Service::new(root.clone(), std::env::current_exe().unwrap()).unwrap();
        let started = Instant::now();
        let observed = service.status("session", &id).await.unwrap();
        assert!(started.elapsed() < Duration::from_millis(500));
        assert_eq!(observed.status, Status::OutcomeUnknown);
        assert!(!observed.supervisor_connected);
        assert_eq!(
            service
                .output("session", &id, 0, 64)
                .await
                .unwrap()
                .data_base64,
            "ZHVyYWJsZSBvdXRwdXQ="
        );
        server.abort();
        drop(service);
        let _ = fs::remove_dir_all(root);
    }
}
