//! Store a smaller copy only when a turn has tool or host-operation details.
use super::*;

pub(in crate::storage) fn migrate(db: &mut Connection) -> Result<()> {
    let tx = db.transaction()?;
    tx.execute_batch(
        "CREATE TABLE IF NOT EXISTS conversation_turns(
            task_id TEXT PRIMARY KEY REFERENCES session_turns(task_id) ON DELETE CASCADE,
            data TEXT NOT NULL
        ) WITHOUT ROWID;",
    )?;
    {
        // Work through legacy projections one at a time. Task snapshots and
        // event sequences are left intact so existing requests stay frozen.
        let mut statement = tx.prepare(
            "SELECT s.data FROM session_turns s
             WHERE NOT EXISTS (SELECT 1 FROM conversation_turns c WHERE c.task_id=s.task_id)",
        )?;
        let mut rows = statement.query([])?;
        while let Some(row) = rows.next()? {
            let data: String = row.get(0)?;
            let mut turn = serde_json::from_str(&data)?;
            record(&tx, &mut turn)?;
        }
    }
    tx.pragma_update(None, "user_version", 22)?;
    tx.commit()?;
    Ok(())
}

pub(super) fn record(tx: &Transaction<'_>, turn: &mut ContextTurn) -> Result<()> {
    let original_count = turn.messages.len();
    turn.messages.retain(|message| {
        matches!(
            message,
            ContextMessage::User { .. } | ContextMessage::Assistant { .. }
        )
    });
    if turn.messages.len() == original_count && turn.host_tools.is_empty() {
        return Ok(());
    }
    turn.host_tools.clear();
    tx.prepare_cached("INSERT INTO conversation_turns(task_id,data) VALUES(?1,?2)")?
        .execute(params![turn.task_id, serde_json::to_string(&turn)?])?;
    Ok(())
}
