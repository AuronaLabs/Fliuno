//! One-shot handoff from the coordinator to a local process supervisor.
//! Native workers may claim or finish a ticket, but only the coordinator
//! commits task transitions. An expired lease cannot be renewed or reclaimed.
use crate::{Error, Kernel, Result, ToolError, ToolOutput, authority_digest};
use fliuno_protocol::{EventKind, Task, TaskStatus, ToolCall, Workspace};
use rusqlite::{Connection, OpenFlags, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    path::PathBuf,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

const STARTUP_MS: i64 = 15_000;
const LEASE_MS: i64 = 4_000;
const MAX_RESULT_BYTES: usize = 1024 * 1024;

pub(crate) fn migrate(db: &Connection) -> Result<()> {
    db.execute_batch(
        "BEGIN IMMEDIATE;
        CREATE TABLE IF NOT EXISTS supervised_tools(
            task_id TEXT PRIMARY KEY REFERENCES tasks(id),
            token TEXT NOT NULL, binding TEXT NOT NULL,
            phase INTEGER NOT NULL CHECK(phase IN (0,1,2)),
            lease_until INTEGER NOT NULL, deadline INTEGER,
            result TEXT, worker_pid INTEGER
        );
        PRAGMA user_version=63; COMMIT;",
    )?;
    Ok(())
}

fn now_ms() -> Result<i64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|d| i64::try_from(d.as_millis()).ok())
        .ok_or_else(|| Error::Conflict("invalid supervision clock".into()))
}

fn binding(task: &Task, workspace: &Workspace, call: &ToolCall, authority: &str) -> String {
    authority_digest(
        "supervised-tool.v1",
        &json!([task.id, task.session_id, workspace, call, authority]),
    )
}

/// Local bootstrap capability. Send only via private IPC, never over RPC,
/// process arguments, or diagnostics. It has no authority to retry a launch.
#[derive(Serialize, Deserialize)]
pub struct SupervisorTicket {
    database: PathBuf,
    task_id: String,
    token: String,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Report {
    Completed(Value),
    Failed(String),
    Unknown(String, Option<Value>),
}

impl Report {
    fn from_result(result: std::result::Result<ToolOutput, ToolError>) -> Self {
        match result {
            Ok(output) if output.artifacts.is_empty() => Self::Completed(output.output),
            Err(ToolError::Failed(message) | ToolError::NeedsCorrection { message, .. }) => {
                Self::Failed(message.chars().take(1024).collect())
            }
            Err(ToolError::OutcomeUnknown(message)) => {
                Self::Unknown(message.chars().take(1024).collect(), None)
            }
            Err(ToolError::OutcomeUnknownWithOutput { message, output }) => {
                Self::Unknown(message.chars().take(1024).collect(), Some(output))
            }
            _ => Self::Unknown("supervisor returned an unsupported result".into(), None),
        }
    }

    fn into_result(self) -> std::result::Result<ToolOutput, ToolError> {
        match self {
            Self::Completed(output) => Ok(ToolOutput {
                output,
                artifacts: vec![],
            }),
            Self::Failed(message) => Err(ToolError::Failed(message)),
            Self::Unknown(message, None) => Err(ToolError::OutcomeUnknown(message)),
            Self::Unknown(message, Some(output)) => {
                Err(ToolError::OutcomeUnknownWithOutput { message, output })
            }
        }
    }
}

pub struct SupervisorLease {
    db: Connection,
    ticket: SupervisorTicket,
}

impl SupervisorTicket {
    fn open(&self, flags: OpenFlags) -> Result<Connection> {
        let db = Connection::open_with_flags(&self.database, flags)?;
        db.busy_timeout(Duration::from_millis(500))?;
        Ok(db)
    }

    /// Atomically validate the frozen identity and acquire the right to perform
    /// one operation. A cancelled Run removes its ticket in the same commit.
    pub fn claim(
        self,
        tool_name: &str,
        authority: &str,
    ) -> Result<(SupervisorLease, Task, Workspace, ToolCall)> {
        let mut db = self.open(OpenFlags::SQLITE_OPEN_READ_WRITE)?;
        db.pragma_update(None, "synchronous", "FULL")?;
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let task: Task = serde_json::from_str(&tx.query_row(
            "SELECT data FROM tasks WHERE id=?1 AND running=1",
            [&self.task_id],
            |r| r.get::<_, String>(0),
        )?)?;
        let workspace: Workspace = serde_json::from_str(&tx.query_row(
            "SELECT w.data FROM workspaces w JOIN sessions s ON s.workspace_id=w.id WHERE s.id=?1",
            [&task.session_id],
            |r| r.get::<_, String>(0),
        )?)?;
        let call = task
            .in_flight_tool
            .as_ref()
            .filter(|call| call.name == tool_name)
            .filter(|_| task.status == TaskStatus::Running)
            .cloned()
            .ok_or_else(|| Error::Conflict("supervised call is no longer active".into()))?;
        let now = now_ms()?;
        let claimed = tx.execute(
            "UPDATE supervised_tools SET phase=1, lease_until=?5, worker_pid=?6
             WHERE task_id=?1 AND token=?2 AND binding=?3 AND phase=0 AND lease_until>?4
               AND (deadline IS NULL OR deadline>?4)",
            params![
                self.task_id,
                self.token,
                binding(&task, &workspace, &call, authority),
                now,
                now + LEASE_MS,
                std::process::id()
            ],
        )?;
        if claimed != 1 {
            return Err(Error::Conflict(
                "supervisor ticket cannot be claimed".into(),
            ));
        }
        tx.commit()?;
        Ok((SupervisorLease { db, ticket: self }, task, workspace, call))
    }

    pub(crate) fn observe(
        &self,
        stopping: &std::sync::atomic::AtomicBool,
    ) -> Option<std::result::Result<ToolOutput, ToolError>> {
        let read = || -> Result<Option<Report>> {
            let db = self.open(OpenFlags::SQLITE_OPEN_READ_ONLY)?;
            let mut statement = db.prepare("SELECT phase,lease_until,result FROM supervised_tools WHERE task_id=?1 AND token=?2")?;
            while !stopping.load(std::sync::atomic::Ordering::Acquire) {
                let mut rows = statement.query(params![self.task_id, self.token])?;
                let Some(row) = rows.next()? else {
                    return Ok(None);
                };
                let phase: i64 = row.get(0)?;
                if phase == 2 {
                    let encoded = row.get_ref(2)?.as_str().map_err(rusqlite::Error::from)?;
                    if encoded.len() > MAX_RESULT_BYTES {
                        return Ok(None);
                    }
                    return Ok(Some(serde_json::from_str(encoded)?));
                }
                if row.get::<_, i64>(1)? <= now_ms()? {
                    return Ok(None);
                }
                drop(rows);
                std::thread::sleep(Duration::from_millis(50));
            }
            Ok(None)
        };
        let report = read();
        if stopping.load(std::sync::atomic::Ordering::Acquire) {
            return None;
        }
        Some(match report {
            Ok(Some(report)) => report.into_result(),
            _ => Err(ToolError::OutcomeUnknown(
                "supervisor stopped without a durable result".into(),
            )),
        })
    }
}

impl SupervisorLease {
    /// False means ownership was withdrawn, the deadline passed, or the lease
    /// expired. The process owner must then stop the command and its tree.
    pub fn renew(&mut self) -> Result<bool> {
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let now = now_ms()?;
        let renewed =
            tx.prepare_cached(
                "UPDATE supervised_tools SET lease_until=?4
             WHERE task_id=?1 AND token=?2 AND phase=1 AND lease_until>?3
               AND (deadline IS NULL OR deadline>?3)",
            )?
            .execute(params![
                self.ticket.task_id,
                self.ticket.token,
                now,
                now + LEASE_MS
            ])? == 1;
        tx.commit()?;
        Ok(renewed)
    }

    pub fn finish(mut self, result: std::result::Result<ToolOutput, ToolError>) -> Result<()> {
        let mut report = serde_json::to_string(&Report::from_result(result))?;
        if report.len() > MAX_RESULT_BYTES {
            report = serde_json::to_string(&Report::Unknown(
                "supervisor result exceeds budget".into(),
                None,
            ))?;
        }
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let now = now_ms()?;
        tx.execute(
            "UPDATE supervised_tools SET phase=2, result=?4
             WHERE task_id=?1 AND token=?2 AND phase=1 AND lease_until>?3
               AND (deadline IS NULL OR deadline>?3)",
            params![self.ticket.task_id, self.ticket.token, now, report],
        )?;
        tx.commit()?;
        Ok(())
    }
}

impl Kernel {
    pub(crate) fn has_supervised_tool(&self, task: &Task) -> Result<bool> {
        if task.status != TaskStatus::Running || task.in_flight_tool.is_none() {
            return Ok(false);
        }
        Ok(self
            .db
            .prepare_cached("SELECT EXISTS(SELECT 1 FROM supervised_tools WHERE task_id=?1)")?
            .query_row([&task.id], |row| row.get(0))?)
    }

    pub(crate) fn supervised_ticket(
        &self,
        task: &Task,
        workspace: &Workspace,
        call: &ToolCall,
        authority: &str,
    ) -> Result<Option<SupervisorTicket>> {
        let token = self
            .db
            .prepare_cached("SELECT token FROM supervised_tools WHERE task_id=?1 AND binding=?2")?
            .query_row(
                params![task.id, binding(task, workspace, call, authority)],
                |row| row.get(0),
            )
            .optional()?;
        Ok(token
            .zip(self.db.path())
            .map(|(token, path)| SupervisorTicket {
                database: path.into(),
                task_id: task.id.clone(),
                token,
            }))
    }

    /// The audit event and permission to start become visible together.
    pub(crate) fn commit_supervisor_start(
        &mut self,
        task: &Task,
        workspace: &Workspace,
        call: &ToolCall,
        authority: &str,
    ) -> Result<SupervisorTicket> {
        let database = self
            .db
            .path()
            .filter(|path| !path.is_empty())
            .ok_or_else(|| Error::Conflict("supervision requires a durable database".into()))?
            .into();
        let token = crate::new_id("sup");
        let event = EventKind::ToolStarted { call: call.clone() };
        let tx = self.db.transaction()?;
        tx.execute(
            "INSERT INTO supervised_tools(task_id,token,binding,phase,lease_until,deadline)
             VALUES(?1,?2,?3,0,?4,(SELECT d.deadline_at_unix_ms FROM run_deadlines d
                 JOIN run_nodes n ON n.run_id=d.run_id WHERE n.task_id=?1))",
            params![
                task.id,
                token,
                binding(task, workspace, call, authority),
                now_ms()? + STARTUP_MS
            ],
        )?;
        let sequences = crate::storage::write_supervisor_start(&tx, task, &event)?;
        tx.commit()?;
        self.publish_task_events(&task.id, &task.session_id, &sequences, &[event]);
        Ok(SupervisorTicket {
            database,
            task_id: task.id.clone(),
            token,
        })
    }

    pub(crate) fn interrupt_supervised_tool(&mut self, task: &Task, reason: &str) -> Result<()> {
        let mut task = task.clone();
        if let Some(call) = &task.in_flight_tool {
            let call_id = call.id.clone();
            task.status = TaskStatus::Interrupted;
            task.output = Some(format!("tool execution result is unknown: {reason}"));
            self.commit(
                &task,
                &[
                    EventKind::ToolOutcomeUnknown {
                        call_id,
                        message: reason.into(),
                        output: None,
                    },
                    EventKind::TaskInterrupted,
                ],
                &[],
            )?;
        }
        Ok(())
    }
}
