//! SQLite snapshots and committed event log. Only this module touches SQL.
mod artifacts;
mod context;
use super::*;
use rusqlite::{OptionalExtension, params};
use serde::de::DeserializeOwned;
use std::{
    fs::{self, OpenOptions},
    path::Path,
};

impl Kernel {
    pub(crate) fn workspace_for_task(&self, task_id: &str) -> Result<Workspace> {
        let data: String = self.db.query_row("SELECT workspaces.data FROM workspaces JOIN sessions ON sessions.workspace_id=workspaces.id JOIN tasks ON tasks.session_id=sessions.id WHERE tasks.id=?1", [task_id], |r| r.get(0))?;
        Ok(serde_json::from_str(&data)?)
    }

    pub(crate) fn require_session(&self, session_id: &str) -> Result<()> {
        let exists: bool = self.db.query_row(
            "SELECT EXISTS(SELECT 1 FROM sessions WHERE id=?1)",
            [session_id],
            |r| r.get(0),
        )?;
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
        if version > 13 {
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
        let mut tools: BTreeMap<String, Arc<dyn Tool>> = BTreeMap::new();
        tools.insert("artifact_create".into(), Arc::new(CreateArtifact));
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

    pub fn create_session(&mut self, workspace_id: &str) -> Result<Session> {
        let exists: bool = self.db.query_row(
            "SELECT EXISTS(SELECT 1 FROM workspaces WHERE id=?1)",
            [workspace_id],
            |r| r.get(0),
        )?;
        if !exists {
            return Err(Error::NotFound("workspace".into()));
        }
        let session = Session {
            id: new_id("session"),
            workspace_id: workspace_id.into(),
        };
        self.db.execute(
            "INSERT INTO sessions VALUES(?1,?2,?3)",
            params![
                session.id,
                session.workspace_id,
                serde_json::to_string(&session)?
            ],
        )?;
        Ok(session)
    }

    pub fn get_task(&self, id: &str) -> Result<Task> {
        let data = self
            .db
            .query_row("SELECT data FROM tasks WHERE id=?1", [id], |r| {
                r.get::<_, String>(0)
            })
            .optional()?
            .ok_or_else(|| Error::NotFound("task".into()))?;
        Ok(serde_json::from_str(&data)?)
    }

    pub fn list_events(&self, task_id: &str, after: u64, limit: u32) -> Result<EventsPage> {
        self.get_task(task_id)?;
        self.query_event_page(
            "SELECT sequence,task_id,data FROM events WHERE task_id=?1 AND sequence>?2 ORDER BY sequence LIMIT ?3",
            params![task_id, validate_event_cursor(after)?, validate_event_limit(limit)?],
            after,
            limit,
        )
    }

    pub fn list_all_events(&self, after: u64, limit: u32) -> Result<EventsPage> {
        self.query_event_page(
            "SELECT sequence,task_id,data FROM events WHERE sequence>?1 ORDER BY sequence LIMIT ?2",
            params![validate_event_cursor(after)?, validate_event_limit(limit)?],
            after,
            limit,
        )
    }

    fn query_event_page<P: rusqlite::Params>(
        &self,
        sql: &str,
        params: P,
        after: u64,
        limit: u32,
    ) -> Result<EventsPage> {
        let mut stmt = self.db.prepare(sql)?;
        let rows = stmt.query_map(params, |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        })?;
        let mut events = Vec::new();
        for row in rows {
            let (sequence, task_id, data) = row?;
            events.push(Event {
                sequence: sequence as u64,
                task_id,
                data: serde_json::from_str(&data)?,
            });
        }
        let has_more = events.len() > limit as usize;
        events.truncate(limit as usize);
        let next_after = events.last().map_or(after, |e| e.sequence);
        Ok(EventsPage {
            events,
            next_after,
            has_more,
        })
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
        let tx = self.db.transaction()?;
        let running = task.status == TaskStatus::Running
            || task.agent_run.as_ref().is_some_and(|run| !run.finished);
        tx.execute("INSERT INTO tasks VALUES(?1,?2,?3,?4) ON CONFLICT(id) DO UPDATE SET running=excluded.running,data=excluded.data",
            params![task.id, task.session_id, running, serde_json::to_string(task)?])?;
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
        if let Some(key) = key {
            tx.execute(
                "INSERT INTO task_requests VALUES(?1,?2,?3)",
                params![task.session_id, key, task.id],
            )?;
        }
        for artifact in artifacts {
            if artifact.task_id != task.id {
                return Err(Error::Invalid("artifact belongs to another task".into()));
            }
            tx.execute(
                "INSERT INTO artifacts VALUES(?1,?2,?3)",
                params![artifact.id, task.id, serde_json::to_string(artifact)?],
            )?;
        }
        let mut pending = Vec::with_capacity(events.len());
        for event in events {
            tx.execute(
                "INSERT INTO events(task_id,data) VALUES(?1,?2)",
                params![task.id, serde_json::to_string(event)?],
            )?;
            let sequence = tx.last_insert_rowid() as u64;
            if matches!(event, EventKind::TaskCompleted { .. }) {
                context::record(&tx, task, sequence)?;
            }
            let persisted = Event {
                sequence,
                task_id: task.id.clone(),
                data: event.clone(),
            };
            pending.push(persisted);
        }
        tx.commit()?;
        self.events.publish(pending);
        Ok(())
    }

    /// Append evidence without rewriting a task snapshot or projecting a turn.
    pub(crate) fn append_task_event(&mut self, task_id: String, data: EventKind) -> Result<()> {
        self.db.execute(
            "INSERT INTO events(task_id,data) VALUES(?1,?2)",
            params![task_id, serde_json::to_string(&data)?],
        )?;
        self.events.publish(vec![Event {
            sequence: self.db.last_insert_rowid() as u64,
            task_id,
            data,
        }]);
        Ok(())
    }

    pub(crate) fn recover(&mut self) -> Result<()> {
        self.interrupt_running_tasks()
    }

    /// Stop scheduling after shutdown or restart without replaying any operation.
    pub fn interrupt_running_tasks(&mut self) -> Result<()> {
        let tasks: Vec<Task> = {
            let mut stmt = self.db.prepare("SELECT data FROM tasks WHERE running=1")?;
            decode_rows(stmt.query_map([], |r| r.get::<_, String>(0))?)?
        };
        for mut task in tasks {
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
        Ok(())
    }
}

fn validate_event_cursor(after: u64) -> Result<i64> {
    i64::try_from(after).map_err(|_| Error::Invalid("after must fit i64".into()))
}

fn validate_event_limit(limit: u32) -> Result<u32> {
    if !(1..=1000).contains(&limit) {
        return Err(Error::Invalid("limit must be 1..1000".into()));
    }
    Ok(limit + 1)
}

fn decode_rows<T: DeserializeOwned>(
    rows: impl Iterator<Item = rusqlite::Result<String>>,
) -> Result<Vec<T>> {
    rows.map(|row| Ok(serde_json::from_str(&row?)?)).collect()
}
