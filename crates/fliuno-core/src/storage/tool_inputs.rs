//! Private continuation tokens and bounded idempotency receipts for tool input.
use super::*;

pub(crate) struct ToolInputRecord {
    pub(crate) call_id: String,
    pub(crate) authority: String,
    pub(crate) request_state: Option<String>,
    pub(crate) round: u8,
    pub(crate) last_request_id: Option<String>,
    pub(crate) last_response_sha256: Option<String>,
}

pub(crate) enum Mutation<'a> {
    Suspend {
        call_id: &'a str,
        authority: &'a str,
        request_state: Option<&'a str>,
        round: u8,
    },
    Resume {
        call_id: &'a str,
        request_id: &'a str,
        response_sha256: &'a str,
    },
}

pub(super) fn migrate(db: &mut Connection) -> Result<()> {
    let tx = db.transaction()?;
    tx.execute_batch(
        "CREATE TABLE IF NOT EXISTS tool_input_continuations(
            task_id TEXT PRIMARY KEY REFERENCES tasks(id) ON DELETE CASCADE,
            call_id TEXT NOT NULL,
            authority TEXT NOT NULL,
            request_state TEXT,
            round INTEGER NOT NULL CHECK(round BETWEEN 1 AND 10),
            last_request_id TEXT,
            last_response_sha256 TEXT
         ) WITHOUT ROWID;",
    )?;
    tx.pragma_update(None, "user_version", 53)?;
    tx.commit()?;
    Ok(())
}

pub(super) fn record(
    tx: &rusqlite::Transaction<'_>,
    task: &Task,
    mutation: Option<&Mutation<'_>>,
) -> Result<()> {
    match mutation {
        Some(Mutation::Suspend {
            call_id,
            authority,
            request_state,
            round,
        }) => {
            tx.prepare_cached(
                "INSERT INTO tool_input_continuations(
                    task_id,call_id,authority,request_state,round,last_request_id,last_response_sha256
                 ) VALUES(?1,?2,?3,?4,?5,NULL,NULL)
                 ON CONFLICT(task_id) DO UPDATE SET
                    call_id=excluded.call_id, authority=excluded.authority,
                    request_state=excluded.request_state, round=excluded.round,
                    last_request_id=CASE WHEN tool_input_continuations.call_id=excluded.call_id
                        THEN tool_input_continuations.last_request_id ELSE NULL END,
                    last_response_sha256=CASE WHEN tool_input_continuations.call_id=excluded.call_id
                        THEN tool_input_continuations.last_response_sha256 ELSE NULL END",
            )?
            .execute(params![task.id, call_id, authority, request_state, round])?;
        }
        Some(Mutation::Resume {
            call_id,
            request_id,
            response_sha256,
        }) => {
            let updated = tx
                .prepare_cached(
                    "UPDATE tool_input_continuations
                 SET request_state=NULL,last_request_id=?3,last_response_sha256=?4
                 WHERE task_id=?1 AND call_id=?2",
                )?
                .execute(params![task.id, call_id, request_id, response_sha256])?;
            if updated != 1 {
                return Err(Error::Conflict(
                    "tool input continuation is no longer available".into(),
                ));
            }
        }
        None if task.status != TaskStatus::WaitingInput => {
            tx.prepare_cached(
                "UPDATE tool_input_continuations SET request_state=NULL WHERE task_id=?1",
            )?
            .execute([&task.id])?;
        }
        None => {}
    }
    Ok(())
}

impl Kernel {
    pub(crate) fn tool_input_record(&self, task_id: &str) -> Result<ToolInputRecord> {
        self.db
            .prepare_cached(
                "SELECT call_id,authority,request_state,round,last_request_id,last_response_sha256
             FROM tool_input_continuations WHERE task_id=?1",
            )?
            .query_row([task_id], |row| {
                Ok(ToolInputRecord {
                    call_id: row.get(0)?,
                    authority: row.get(1)?,
                    request_state: row.get(2)?,
                    round: row.get::<_, i64>(3)? as u8,
                    last_request_id: row.get(4)?,
                    last_response_sha256: row.get(5)?,
                })
            })
            .optional()?
            .ok_or_else(|| Error::Conflict("tool input continuation is unavailable".into()))
    }
}
