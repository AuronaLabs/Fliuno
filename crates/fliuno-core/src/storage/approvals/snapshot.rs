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

pub(in crate::storage) fn migrate_snapshot_open_indexes(db: &mut Connection) -> Result<()> {
    let tx = db.transaction()?;
    tx.execute_batch(
        "CREATE INDEX IF NOT EXISTS pending_approvals_opened ON pending_approvals(opened_through);
         CREATE INDEX IF NOT EXISTS closed_approvals_opened ON closed_approvals(opened_through);
         PRAGMA user_version=52;",
    )?;
    tx.commit()?;
    Ok(())
}

// Prefer a short rowid interval to sorting approvals that closed after the
// snapshot, especially once a scoped inbox has accumulated many newer rows.
const SHORT_APPROVAL_SEQUENCE_RANGE: i64 = 4096;

impl Kernel {
    pub fn list_pending_approval_snapshot(
        &self,
        request: &ApprovalInboxListParams,
    ) -> Result<ApprovalInboxPage> {
        let tx = self.db.unchecked_transaction()?;
        let page = self.read_pending_approval_snapshot(request)?;
        tx.commit()?;
        Ok(page)
    }

    /// The caller pins the watermark and approval indexes to one read view.
    pub(in crate::storage) fn read_pending_approval_snapshot(
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
            let through = snapshot.through_sequence as i64;
            let upper = self.approval_sequence_through(through)?;
            let closed_table =
                if upper.saturating_sub(after as i64) <= SHORT_APPROVAL_SEQUENCE_RANGE {
                    "closed_approvals NOT INDEXED"
                } else {
                    "closed_approvals"
                };
            // Bound both inputs to the merge. The upper inbox cursor excludes
            // approvals opened after the snapshot even if they have since closed.
            let sql = format!(
                "WITH active AS (
                     SELECT {COLUMNS} FROM pending_approvals WHERE sequence>?2 AND sequence<=?6 {filter}
                     AND opened_through<=?5 AND (expires_at IS NULL OR expires_at>?3)
                     ORDER BY sequence LIMIT ?4
                 ), closed AS (
                     SELECT {COLUMNS} FROM {closed_table} WHERE sequence>?2 AND sequence<=?6 {filter}
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
                through,
                upper
            ])?;
            read_approval_page(rows, after, limit, target_bytes)?
        };
        Ok(ApprovalInboxPage { snapshot, page })
    }

    fn approval_sequence_through(&self, through: i64) -> Result<i64> {
        // The inbox sequence is assigned at insertion and its open stamp is
        // the monotone committed event watermark. When rows close, both keys
        // move together. Each index ends in the rowid, which is `sequence`.
        Ok(self
            .db
            .prepare_cached(
                "SELECT max(
               coalesce((SELECT sequence FROM pending_approvals WHERE opened_through<=?1
                         ORDER BY opened_through DESC,sequence DESC LIMIT 1),0),
               coalesce((SELECT sequence FROM closed_approvals WHERE opened_through<=?1
                         ORDER BY opened_through DESC,sequence DESC LIMIT 1),0))",
            )?
            .query_row([through], |row| row.get(0))?)
    }
}
