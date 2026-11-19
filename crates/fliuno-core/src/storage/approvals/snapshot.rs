//! Retain compact closed rows so a partially consumed inbox can be read after reconnect.
use super::*;

pub(in crate::storage) fn migrate_snapshots(db: &mut Connection) -> Result<()> {
    let tx = db.transaction()?;
    tx.execute_batch(
        "CREATE TABLE IF NOT EXISTS approval_inbox_meta(
            id INTEGER PRIMARY KEY CHECK(id=1), first_through INTEGER NOT NULL
         );
         INSERT INTO approval_inbox_meta(id,first_through)
         SELECT 1,COALESCE(MAX(sequence),0) FROM events WHERE true
         ON CONFLICT(id) DO NOTHING;
         CREATE TABLE IF NOT EXISTS closed_approvals(
            sequence INTEGER PRIMARY KEY,
            approval_id TEXT NOT NULL,
            task_id TEXT NOT NULL,
            session_id TEXT NOT NULL,
            workspace_id TEXT NOT NULL,
            kind INTEGER NOT NULL,
            allow_once INTEGER NOT NULL,
            expires_at INTEGER,
            opened_through INTEGER NOT NULL,
            closed_through INTEGER NOT NULL
         );
         CREATE INDEX IF NOT EXISTS closed_approvals_through ON closed_approvals(closed_through);
         CREATE INDEX IF NOT EXISTS closed_approvals_session ON closed_approvals(session_id,closed_through);
         CREATE INDEX IF NOT EXISTS closed_approvals_workspace ON closed_approvals(workspace_id,closed_through);",
    )?;
    let has_open_stamp: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_info('pending_approvals') WHERE name='opened_through')",
        [],
        |row| row.get(0),
    )?;
    if !has_open_stamp {
        tx.execute_batch(
            "ALTER TABLE pending_approvals ADD COLUMN opened_through INTEGER NOT NULL DEFAULT 0;
             UPDATE pending_approvals SET opened_through=(SELECT first_through FROM approval_inbox_meta WHERE id=1);",
        )?;
    }
    // The events and mutable inbox are updated in that order in the same transaction.
    tx.execute_batch(
        "CREATE TRIGGER IF NOT EXISTS stamp_pending_approval AFTER INSERT ON pending_approvals BEGIN
            UPDATE pending_approvals SET opened_through=(SELECT COALESCE(MAX(sequence),0) FROM events)
            WHERE sequence=NEW.sequence;
         END;
         CREATE TRIGGER IF NOT EXISTS retain_closed_approval AFTER DELETE ON pending_approvals BEGIN
            INSERT INTO closed_approvals(sequence,approval_id,task_id,session_id,workspace_id,
                kind,allow_once,expires_at,opened_through,closed_through)
            VALUES(OLD.sequence,OLD.approval_id,OLD.task_id,OLD.session_id,OLD.workspace_id,
                OLD.kind,OLD.allow_once,OLD.expires_at,OLD.opened_through,
                (SELECT COALESCE(MAX(sequence),0) FROM events));
         END;
         PRAGMA user_version=41;",
    )?;
    tx.commit()?;
    Ok(())
}

impl Kernel {
    pub fn list_pending_approval_snapshot(
        &self,
        request: &ApprovalInboxListParams,
    ) -> Result<ApprovalInboxPage> {
        let (scope, filter) = self.approval_page_scope(&request.page)?;
        let ApprovalListParams {
            after,
            limit,
            target_bytes,
            ..
        } = request.page;
        let watermark = self.event_watermark()?;
        let snapshot = if let Some(snapshot) = request.snapshot {
            let baseline: i64 = self
                .db
                .prepare_cached("SELECT first_through FROM approval_inbox_meta WHERE id=1")?
                .query_row([], |row| row.get(0))?;
            if !(baseline as u64..=watermark).contains(&snapshot.through_sequence) {
                return Err(Error::Invalid(
                    "approval snapshot watermark is not available".into(),
                ));
            }
            snapshot
        } else {
            if after != 0 {
                return Err(Error::Invalid(
                    "approval snapshot is required when continuing a page".into(),
                ));
            }
            ApprovalInboxSnapshot {
                through_sequence: watermark,
                captured_at_unix_ms: agent::now_ms()?,
            }
        };
        let time = sql_timestamp(snapshot.captured_at_unix_ms)?;
        const COLUMNS: &str = "sequence,approval_id,task_id,session_id,kind,allow_once,expires_at";
        let page = if snapshot.through_sequence == watermark {
            // No closed rows can be visible at the current durable watermark.
            let sql = format!(
                "SELECT {COLUMNS} FROM pending_approvals WHERE sequence>?2 {filter}
                 AND (expires_at IS NULL OR expires_at>?3) ORDER BY sequence LIMIT ?4"
            );
            let mut select = self.db.prepare_cached(&sql)?;
            let rows = select.query(params![scope, after as i64, time, limit + 1])?;
            read_approval_page(rows, after, limit, target_bytes)?
        } else {
            // Bound each sorted input to the merge. Only closed rows after the
            // snapshot can contribute; the scope/close indexes can seek there.
            let sql = format!(
                "WITH active AS (
                     SELECT {COLUMNS} FROM pending_approvals WHERE sequence>?2 {filter}
                     AND opened_through<=?5 AND (expires_at IS NULL OR expires_at>?3)
                     ORDER BY sequence LIMIT ?4
                 ), closed AS (
                     SELECT {COLUMNS} FROM closed_approvals WHERE sequence>?2 {filter}
                     AND closed_through>?5 AND opened_through<=?5
                     AND (expires_at IS NULL OR expires_at>?3)
                     ORDER BY sequence LIMIT ?4
                 ) SELECT * FROM active UNION ALL SELECT * FROM closed ORDER BY sequence LIMIT ?4"
            );
            let mut select = self.db.prepare_cached(&sql)?;
            let rows = select.query(params![
                scope,
                after as i64,
                time,
                limit + 1,
                snapshot.through_sequence as i64
            ])?;
            read_approval_page(rows, after, limit, target_bytes)?
        };
        Ok(ApprovalInboxPage { snapshot, page })
    }
}
