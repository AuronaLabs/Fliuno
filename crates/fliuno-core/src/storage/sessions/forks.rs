//! A fork inherits a fixed, bounded set of immutable conversation projections.
use super::*;
use rusqlite::TransactionBehavior;

pub(in crate::storage) fn migrate(db: &mut Connection) -> Result<()> {
    let tx = db.transaction()?;
    tx.execute_batch(
        "CREATE TABLE IF NOT EXISTS session_fork_requests(
            source_session_id TEXT NOT NULL REFERENCES sessions(id),
            request_key TEXT NOT NULL,
            request TEXT NOT NULL,
            fork_id TEXT NOT NULL REFERENCES sessions(id),
            PRIMARY KEY(source_session_id,request_key)
        ) WITHOUT ROWID;
        CREATE TABLE IF NOT EXISTS session_fork_turns(
            session_id TEXT NOT NULL REFERENCES sessions(id),
            sequence INTEGER NOT NULL REFERENCES session_turns(sequence),
            PRIMARY KEY(session_id,sequence)
        ) WITHOUT ROWID;
        CREATE VIEW IF NOT EXISTS session_context_turns AS
            SELECT task_id,session_id,sequence,data FROM session_turns
            UNION ALL
            SELECT t.task_id,f.session_id,f.sequence,COALESCE(c.data,t.data)
            FROM session_fork_turns f CROSS JOIN session_turns t ON t.sequence=f.sequence
            LEFT JOIN conversation_turns c ON c.task_id=t.task_id;
        PRAGMA user_version=76;",
    )?;
    tx.commit()?;
    Ok(())
}

impl Kernel {
    pub fn fork_session(&mut self, request: &SessionForkParams) -> Result<Session> {
        validate_key(&request.client_request_id)?;
        let title = request
            .title
            .as_deref()
            .map(validate_session_title)
            .transpose()?;
        if request.through_sequence.is_some() && request.through_task_id.is_some() {
            return Err(Error::Invalid(
                "choose a completion sequence or a source task".into(),
            ));
        }
        let identity =
            serde_json::to_string(&(title, &request.through_task_id, request.through_sequence))?;
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let existing = tx.query_row(
            "SELECT request,fork_id FROM session_fork_requests WHERE source_session_id=?1 AND request_key=?2",
            params![request.session_id, request.client_request_id],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
        ).optional()?;
        if let Some((saved, id)) = existing {
            if saved != identity {
                return Err(Error::Conflict(
                    "clientRequestId was used with different fork options".into(),
                ));
            }
            return read_session(
                &mut tx.prepare("SELECT data FROM sessions WHERE id=?1")?,
                &id,
            );
        }
        let source = read_session(
            &mut tx.prepare("SELECT data FROM sessions WHERE id=?1")?,
            &request.session_id,
        )?;
        let watermark: i64 =
            tx.query_row("SELECT COALESCE(MAX(sequence),0) FROM events", [], |r| {
                r.get(0)
            })?;
        let through = if let Some(task) = &request.through_task_id {
            tx.query_row(
                "SELECT sequence FROM session_context_turns WHERE session_id=?1 AND task_id=?2",
                params![source.id, task],
                |r| r.get::<_, i64>(0),
            )
            .optional()?
            .ok_or_else(|| Error::NotFound("completed task in source session".into()))?
        } else if let Some(through) = request.through_sequence {
            let requested = validate_event_cursor(through)?;
            if requested > watermark {
                return Err(Error::Invalid(
                    "throughSequence exceeds the current event sequence".into(),
                ));
            }
            requested
        } else {
            watermark
        };
        let turns = tx
            .prepare_cached(
                "SELECT sequence FROM (
                 SELECT sequence FROM session_turns WHERE session_id=?1 AND sequence<=?2
                 UNION ALL
                 SELECT sequence FROM session_fork_turns WHERE session_id=?1 AND sequence<=?2
             ) ORDER BY sequence DESC LIMIT ?3",
            )?
            .query_map(params![source.id, through, MAX_SESSION_FORK_TURNS], |r| {
                r.get::<_, i64>(0)
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let session = Session {
            id: new_id("session"),
            workspace_id: source.workspace_id,
            title: title.map(str::to_owned),
            archived: false,
            fork_origin: Some(SessionForkOrigin {
                session_id: source.id,
                through_sequence: through as u64,
                inherited_turns: turns.len() as u32,
            }),
        };
        tx.execute(
            "INSERT INTO sessions VALUES(?1,?2,?3)",
            params![
                session.id,
                session.workspace_id,
                serde_json::to_string(&session)?
            ],
        )?;
        for sequence in turns {
            tx.prepare_cached("INSERT INTO session_fork_turns VALUES(?1,?2)")?
                .execute(params![session.id, sequence])?;
        }
        tx.execute(
            "INSERT INTO session_fork_requests VALUES(?1,?2,?3,?4)",
            params![
                request.session_id,
                request.client_request_id,
                identity,
                session.id
            ],
        )?;
        tx.commit()?;
        Ok(session)
    }
}
