//! SQLite snapshots and committed event log. Only this module touches SQL.
mod approval_decisions;
mod approval_runs;
mod approvals;
mod artifacts;
mod context;
mod model_usage;
mod remote_checks;
mod replay;
mod run_boundaries;
mod sessions;
mod task_cancel;
mod task_changes;
mod task_summaries;
mod work;
mod workspaces;
use super::*;
pub use replay::MAX_EVENT_TARGET_BYTES;
use rusqlite::{OptionalExtension, params};
use std::{
    fs::{self, OpenOptions},
    path::Path,
};

pub(crate) const SCHEMA_VERSION: u32 = 43;
const RECOVERY_BATCH_SIZE: i64 = 64;
const INSERT_EVENT: &str = "INSERT INTO events(task_id,data) VALUES(?1,?2)";

fn read_task(db: &Connection, id: &str) -> Result<Task> {
    let mut statement = db.prepare_cached("SELECT data FROM tasks WHERE id=?1")?;
    let mut rows = statement.query([id])?;
    let row = rows.next()?.ok_or_else(|| Error::NotFound("task".into()))?;
    let data = row.get_ref(0)?.as_str().map_err(rusqlite::Error::from)?;
    Ok(serde_json::from_str(data)?)
}

/// Write all projections in the caller's transaction. Publish the returned
/// sequences only after that transaction successfully commits.
fn write_transition(
    tx: &rusqlite::Transaction<'_>,
    task: &Task,
    events: &[EventKind],
    artifacts: &[Artifact],
    key: Option<&str>,
    resume: Option<&ApprovalResumeParams>,
) -> Result<Vec<u64>> {
    let running = task.status == TaskStatus::Running
        || task.agent_run.as_ref().is_some_and(|run| !run.finished);
    tx.prepare_cached(
        "INSERT INTO tasks VALUES(?1,?2,?3,?4)
         ON CONFLICT(id) DO UPDATE SET running=excluded.running,data=excluded.data",
    )?
    .execute(params![
        task.id,
        task.session_id,
        running,
        serde_json::to_string(task)?
    ])?;
    task_summaries::record(tx, task)?;
    if let Some(continuation) = &task.remote_continuation
        && continuation.expected.state == AgentTaskState::InputRequired
    {
        if key.is_some() {
            let inserted = tx.execute("INSERT INTO remote_continuation_claims VALUES(?1,?2) ON CONFLICT(source_task_id) DO NOTHING",
                params![continuation.source_task_id, task.id])?;
            if inserted == 0 {
                return Err(Error::Conflict(
                    "this paused turn already has a continuation".into(),
                ));
            }
        } else if !running && !continuation.submission_started {
            tx.execute("DELETE FROM remote_continuation_claims WHERE source_task_id=?1 AND attempt_task_id=?2",
                params![continuation.source_task_id, task.id])?;
        }
    }
    if let Some(continuation) = &task.agent_session_continuation {
        if key.is_some() {
            let claimed = tx.prepare_cached(
                "INSERT INTO agent_session_continuation_claims VALUES(?1,?2) ON CONFLICT(source_task_id) DO NOTHING",
            )?.execute(params![continuation.source_task_id, task.id])?;
            if claimed == 0 {
                return Err(Error::Conflict(
                    "this agent turn already has a continuation".into(),
                ));
            }
        } else if !running && !continuation.submission_started {
            tx.prepare_cached(
                "DELETE FROM agent_session_continuation_claims WHERE source_task_id=?1 AND attempt_task_id=?2",
            )?.execute(params![continuation.source_task_id, task.id])?;
        }
    }
    if let Some(key) = key {
        tx.execute(
            "INSERT INTO task_requests VALUES(?1,?2,?3)",
            params![task.session_id, key, task.id],
        )?;
    }
    for artifact in artifacts {
        artifacts::record(tx, &task.id, artifact)?;
    }
    let mut sequences = Vec::with_capacity(events.len());
    if !events.is_empty() {
        let mut insert = tx.prepare_cached(INSERT_EVENT)?;
        for event in events {
            insert.execute(params![task.id, serde_json::to_string(event)?])?;
            let sequence = tx.last_insert_rowid();
            if matches!(event, EventKind::TaskCompleted { .. }) {
                context::record(tx, task, sequence as u64, true)?;
            }
            approval_decisions::record(tx, &task.id, event, sequence)?;
            run_boundaries::record(tx, &task.id, event, sequence)?;
            if let (Some(resume), EventKind::ApprovalResolved { .. }) = (resume, event) {
                approval_runs::record(tx, &task.id, resume, sequence)?;
            }
            if let EventKind::ModelUsageReported { usage, .. } = event {
                model_usage::record(tx, task, sequence, usage)?;
            }
            sequences.push(sequence as u64);
        }
    }
    // Inbox transitions are stamped with the final event in this transaction.
    approvals::record(tx, task)?;
    Ok(sequences)
}

impl Kernel {
    pub(crate) fn workspace_for_task(&self, task_id: &str) -> Result<Workspace> {
        let data: String = self.db.query_row("SELECT workspaces.data FROM workspaces JOIN sessions ON sessions.workspace_id=workspaces.id JOIN tasks ON tasks.session_id=sessions.id WHERE tasks.id=?1", [task_id], |r| r.get(0))?;
        Ok(serde_json::from_str(&data)?)
    }

    pub(crate) fn require_task(&self, task_id: &str) -> Result<()> {
        let exists: bool = self
            .db
            .prepare_cached("SELECT EXISTS(SELECT 1 FROM tasks WHERE id=?1)")?
            .query_row([task_id], |row| row.get(0))?;
        if exists {
            Ok(())
        } else {
            Err(Error::NotFound("task".into()))
        }
    }

    pub(crate) fn require_workspace(&self, workspace_id: &str) -> Result<()> {
        let exists: bool = self
            .db
            .prepare_cached("SELECT EXISTS(SELECT 1 FROM workspaces WHERE id=?1)")?
            .query_row([workspace_id], |row| row.get(0))?;
        if exists {
            Ok(())
        } else {
            Err(Error::NotFound("workspace".into()))
        }
    }

    pub(crate) fn require_session(&self, session_id: &str) -> Result<()> {
        let exists: bool = self
            .db
            .prepare_cached("SELECT EXISTS(SELECT 1 FROM sessions WHERE id=?1)")?
            .query_row([session_id], |row| row.get(0))?;
        if exists {
            Ok(())
        } else {
            Err(Error::NotFound("session".into()))
        }
    }

    pub fn open(path: impl AsRef<Path>, provider: Box<dyn ModelProvider>) -> Result<Self> {
        let path = path.as_ref();
        let parent = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        fs::create_dir_all(parent)?;
        let absolute = if path.exists() {
            fs::canonicalize(path)?
        } else {
            fs::canonicalize(parent)?.join(
                path.file_name()
                    .ok_or_else(|| Error::Invalid("database filename required".into()))?,
            )
        };
        let mut lock_path = absolute.as_os_str().to_os_string();
        lock_path.push(".lock");
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(lock_path)?;
        lock.try_lock()
            .map_err(|e| Error::Conflict(format!("cannot lock database: {e}")))?;
        Self::from_connection(Connection::open(absolute)?, provider, Some(lock))
    }

    pub fn in_memory(provider: Box<dyn ModelProvider>) -> Result<Self> {
        Self::from_connection(Connection::open_in_memory()?, provider, None)
    }

    pub(crate) fn from_connection(
        mut db: Connection,
        provider: Box<dyn ModelProvider>,
        lock: Option<File>,
    ) -> Result<Self> {
        let version: u32 = db.pragma_query_value(None, "user_version", |r| r.get(0))?;
        if version > SCHEMA_VERSION {
            return Err(Error::Conflict(
                "database schema is newer than this binary".into(),
            ));
        }
        db.execute_batch(
            "PRAGMA foreign_keys=ON; PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;",
        )?;
        if version == 0 {
            db.execute_batch("
            BEGIN IMMEDIATE;
            CREATE TABLE IF NOT EXISTS workspaces(id TEXT PRIMARY KEY, root TEXT NOT NULL UNIQUE, data TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS sessions(id TEXT PRIMARY KEY, workspace_id TEXT NOT NULL REFERENCES workspaces(id), data TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS tasks(id TEXT PRIMARY KEY, session_id TEXT NOT NULL REFERENCES sessions(id), running INTEGER NOT NULL, data TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS artifacts(id TEXT PRIMARY KEY, task_id TEXT NOT NULL REFERENCES tasks(id), data TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS events(sequence INTEGER PRIMARY KEY AUTOINCREMENT, task_id TEXT NOT NULL REFERENCES tasks(id), data TEXT NOT NULL);
            CREATE INDEX IF NOT EXISTS events_task_sequence ON events(task_id, sequence);
            CREATE INDEX IF NOT EXISTS artifacts_task ON artifacts(task_id);
            PRAGMA user_version=1; COMMIT;")?;
        }
        if version < 2 {
            db.execute_batch("BEGIN IMMEDIATE;
                CREATE TABLE task_requests(session_id TEXT NOT NULL REFERENCES sessions(id), request_key TEXT NOT NULL, task_id TEXT NOT NULL REFERENCES tasks(id), PRIMARY KEY(session_id,request_key));
                PRAGMA user_version=2; COMMIT;")?;
        }
        if version < 3 {
            // Agent snapshots and recoverable waiting states must not be opened
            // by older binaries which know only the model/tool task machine.
            db.pragma_update(None, "user_version", 3)?;
        }
        if version < 4 {
            db.pragma_update(None, "user_version", 4)?;
        }
        if version < 5 {
            context::migrate(&mut db)?;
        }
        if version < 6 {
            // Older binaries cannot decode structured uncertain tool evidence.
            db.pragma_update(None, "user_version", 6)?;
        }
        if version < 7 {
            // Remote task/artifact evidence and new stop reasons must not be
            // recovered by older binaries which cannot decode those snapshots.
            db.pragma_update(None, "user_version", 7)?;
        }
        if version < 8 {
            // Legacy pending approvals/queued Agents lack registration binding.
            // Keep them inspectable, but an absent digest cannot authorize IO.
            db.pragma_update(None, "user_version", 8)?;
        }
        if version < 9 {
            // Explicit history selection adds a frozen input contract that
            // older binaries cannot decode; existing snapshots stay unchanged.
            db.pragma_update(None, "user_version", 9)?;
        }
        if version < 10 {
            // Older readers do not understand the remote-check event variant.
            db.pragma_update(None, "user_version", 10)?;
        }
        if version < 11 {
            db.execute_batch(
                "BEGIN IMMEDIATE;
                CREATE TABLE IF NOT EXISTS remote_continuation_claims(
                    source_task_id TEXT PRIMARY KEY REFERENCES tasks(id),
                    attempt_task_id TEXT NOT NULL REFERENCES tasks(id)
                );
                PRAGMA user_version=11; COMMIT;",
            )?;
        }
        if version < 12 {
            // Rowid is the trailing key of each scope index. Recovery only
            // indexes tasks that still have unfinished work.
            db.execute_batch(
                "BEGIN IMMEDIATE;
                CREATE INDEX IF NOT EXISTS tasks_session ON tasks(session_id);
                CREATE INDEX IF NOT EXISTS sessions_workspace ON sessions(workspace_id);
                CREATE INDEX IF NOT EXISTS tasks_running ON tasks(running) WHERE running=1;
                PRAGMA user_version=12; COMMIT;",
            )?;
        }
        if version < 13 {
            // Older readers cannot decode the opt-in selection in frozen input.
            db.pragma_update(None, "user_version", 13)?;
        }
        if version < 14 {
            context::migrate_turn_stats(&mut db)?;
        }
        if version < 15 {
            // Named session snapshots add a field older readers cannot decode.
            db.pragma_update(None, "user_version", 15)?;
        }
        if version < 16 {
            replay::migrate_session_index(&mut db)?;
        }
        if version < 17 {
            sessions::migrate_requests(&mut db)?;
        }
        if version < 18 {
            task_summaries::migrate(&mut db)?;
        }
        if version < 19 {
            sessions::migrate_archive_index(&mut db)?;
        }
        if version < 20 {
            approvals::migrate(&mut db)?;
        }
        if version < 21 {
            artifacts::migrate(&mut db)?;
        }
        if version < 22 {
            context::migrate_conversations(&mut db)?;
        }
        if version < 23 {
            task_summaries::migrate_status_index(&mut db)?;
        }
        if version < 24 {
            task_summaries::migrate_workspace_scope(&mut db)?;
        }
        if version < 25 {
            db.execute_batch(
                "BEGIN;
                CREATE TABLE IF NOT EXISTS agent_session_continuation_claims(
                    source_task_id TEXT PRIMARY KEY REFERENCES tasks(id),
                    attempt_task_id TEXT NOT NULL REFERENCES tasks(id)
                ) WITHOUT ROWID;
                PRAGMA user_version=25;
                COMMIT;",
            )?;
        }
        if version < 26 {
            artifacts::migrate_catalog(&mut db)?;
        }
        if version < 27 {
            replay::migrate_workspace_index(&mut db)?;
        }
        if version < 28 {
            artifacts::migrate_catalog_media_type(&mut db)?;
        }
        if version < 29 {
            remote_checks::migrate(&mut db)?;
        }
        if version < 30 {
            // Older readers cannot decode match_recent in frozen task snapshots.
            db.pragma_update(None, "user_version", 30)?;
        }
        if version < 31 {
            // Older readers cast all artifact bodies to UTF-8.
            db.pragma_update(None, "user_version", 31)?;
        }
        if version < 32 {
            // Older readers reject partIndex in persisted agent artifact links.
            db.pragma_update(None, "user_version", 32)?;
        }
        if version < 33 {
            context::migrate_workspace_turns(&mut db)?;
        }
        if version < 34 {
            approval_decisions::migrate(&mut db)?;
        }
        if version < 35 {
            workspaces::migrate(&mut db)?;
        }
        if version < 36 {
            // Older kernels cannot replay model_usage_reported events.
            db.pragma_update(None, "user_version", 36)?;
        }
        if version < 38 {
            approval_runs::migrate(&mut db)?;
        }
        if version < 39 {
            run_boundaries::migrate(&mut db)?;
        }
        if version < 40 {
            model_usage::migrate(&mut db)?;
        }
        if version < 41 {
            approvals::migrate_snapshots(&mut db)?;
        }
        if version < 42 {
            work::migrate(&mut db)?;
        }
        if version < 43 {
            model_usage::migrate_totals(&mut db)?;
        }
        let mut tools: BTreeMap<String, Arc<dyn Tool>> = BTreeMap::new();
        tools.insert("artifact_create".into(), Arc::new(CreateArtifact));
        tools.insert(
            "artifact_create_binary".into(),
            Arc::new(crate::tool::CreateBinaryArtifact),
        );
        let mut kernel = Self {
            db,
            provider: std::sync::Arc::from(provider),
            tools,
            agents: BTreeMap::new(),
            agent_controls: BTreeMap::new(),
            policy: Box::new(AskForWrites),
            events: super::events::EventHub::new(),
            progress: super::events::ProgressHub::new(),
            _lock: lock,
        };
        kernel.recover()?;
        Ok(kernel)
    }

    pub fn open_workspace(&mut self, root: &str) -> Result<Workspace> {
        let root = fs::canonicalize(root)?;
        if !root.is_dir() {
            return Err(Error::Invalid("workspace root must be a directory".into()));
        }
        let root = root
            .into_os_string()
            .into_string()
            .map_err(|_| Error::Invalid("workspace root must be UTF-8".into()))?;
        if let Some(data) = self
            .db
            .query_row("SELECT data FROM workspaces WHERE root=?1", [&root], |r| {
                r.get::<_, String>(0)
            })
            .optional()?
        {
            return Ok(serde_json::from_str(&data)?);
        }
        let workspace = Workspace {
            id: new_id("workspace"),
            root,
        };
        self.db.execute(
            "INSERT INTO workspaces VALUES(?1,?2,?3)",
            params![
                workspace.id,
                workspace.root,
                serde_json::to_string(&workspace)?
            ],
        )?;
        Ok(workspace)
    }

    pub fn get_task(&self, id: &str) -> Result<Task> {
        read_task(&self.db, id)
    }

    pub(crate) fn commit(
        &mut self,
        task: &Task,
        events: &[EventKind],
        artifacts: &[Artifact],
    ) -> Result<()> {
        self.commit_with_request(task, events, artifacts, None)
    }

    pub(crate) fn commit_with_request(
        &mut self,
        task: &Task,
        events: &[EventKind],
        artifacts: &[Artifact],
        key: Option<&str>,
    ) -> Result<()> {
        self.commit_with_bindings(task, events, artifacts, key, None)
    }

    pub(crate) fn commit_with_bindings(
        &mut self,
        task: &Task,
        events: &[EventKind],
        artifacts: &[Artifact],
        key: Option<&str>,
        resume: Option<&ApprovalResumeParams>,
    ) -> Result<()> {
        let tx = self.db.transaction()?;
        let sequences = write_transition(&tx, task, events, artifacts, key, resume)?;
        tx.commit()?;
        self.publish_task_events(&task.id, &task.session_id, &sequences, events);
        Ok(())
    }

    fn publish_task_events(
        &self,
        task_id: &str,
        session_id: &str,
        sequences: &[u64],
        events: &[EventKind],
    ) {
        self.events.publish(
            task_id,
            session_id,
            sequences,
            |index, sequence| Event {
                sequence,
                task_id: task_id.to_owned(),
                data: events[index].clone(),
            },
            || {
                Ok(self
                    .db
                    .prepare_cached("SELECT workspace_id FROM sessions WHERE id=?1")?
                    .query_row([session_id], |row| row.get(0))?)
            },
        );
    }

    pub(crate) fn recover(&mut self) -> Result<()> {
        self.interrupt_running_tasks()
    }

    /// Stop scheduling after shutdown or restart without replaying any operation.
    pub fn interrupt_running_tasks(&mut self) -> Result<()> {
        let mut after = 0_i64;
        loop {
            // The running index covers row IDs. Keep only those IDs in the
            // page so large frozen contexts cannot multiply recovery memory.
            let rowids: Vec<i64> = {
                let mut stmt = self.db.prepare_cached(
                    "SELECT rowid FROM tasks WHERE running=1 AND rowid>?1 ORDER BY rowid LIMIT ?2",
                )?;
                stmt.query_map(params![after, RECOVERY_BATCH_SIZE], |row| row.get(0))?
                    .collect::<std::result::Result<_, _>>()?
            };
            if rowids.is_empty() {
                break;
            }
            for rowid in rowids {
                after = rowid;
                let mut task: Task = {
                    let mut stmt = self
                        .db
                        .prepare_cached("SELECT data FROM tasks WHERE rowid=?1 AND running=1")?;
                    let mut rows = stmt.query([rowid])?;
                    let Some(row) = rows.next()? else { continue };
                    let data = row.get_ref(0)?.as_str().map_err(rusqlite::Error::from)?;
                    serde_json::from_str(data)?
                };
                let mut events = Vec::new();
                if task.status != TaskStatus::Cancelled {
                    task.status = TaskStatus::Interrupted;
                    events.push(EventKind::TaskInterrupted);
                }
                if let Some(run) = &mut task.agent_run {
                    agent_tools::close_host_tools(run);
                    run.finished = true;
                    if task
                        .remote_continuation
                        .as_ref()
                        .is_some_and(|c| !c.submission_started)
                    {
                        run.outcome_unknown = false;
                    }
                    events.extend(agent::close_approvals(run));
                    events.push(EventKind::AgentFinished {
                        run_id: run.id.clone(),
                        stop_reason: run.stop_reason,
                        outcome_unknown: run.outcome_unknown,
                    });
                }
                if let Some(control) = self.agent_controls.remove(&task.id) {
                    control.cancellation.cancel();
                }
                if task.model_outcome_unknown {
                    events.push(EventKind::ModelOutcomeUnknown {
                        step: task.model_steps,
                    });
                }
                self.commit(&task, &events, &[])?;
            }
        }
        Ok(())
    }
}

fn validate_event_cursor(after: u64) -> Result<i64> {
    i64::try_from(after).map_err(|_| Error::Invalid("after must fit i64".into()))
}
