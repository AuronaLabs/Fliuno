//! Compact retry receipts for ordinary task starts. Older keys, approvals and
//! external continuations still use their original identity checks.
use super::*;
use crate::task::start::{StartLookup, StartRequest, request_conflict};
use sha2::{Digest, Sha256};

pub(super) fn migrate(db: &mut Connection) -> Result<()> {
    let tx = db.transaction()?;
    tx.execute_batch(
        "CREATE TABLE IF NOT EXISTS task_start_receipts(
            session_id TEXT NOT NULL,
            request_key TEXT NOT NULL,
            input_sha256 BLOB NOT NULL CHECK(length(input_sha256)=32),
            options TEXT NOT NULL,
            PRIMARY KEY(session_id,request_key),
            FOREIGN KEY(session_id,request_key) REFERENCES task_requests(session_id,request_key)
         ) WITHOUT ROWID;
         PRAGMA user_version=44;",
    )?;
    tx.commit()?;
    Ok(())
}

pub(super) struct Receipt {
    digest: [u8; 32],
    options: String,
}

impl Receipt {
    pub(super) fn new(task: &Task, input: &str) -> Result<Self> {
        let context = task
            .context
            .as_ref()
            .ok_or_else(|| Error::Invalid("task start requires frozen context".into()))?;
        Ok(Self {
            digest: Sha256::digest(input.as_bytes()).into(),
            options: serde_json::to_string(&(&task.backend, &context.options))?,
        })
    }

    pub(super) fn record(
        &self,
        tx: &rusqlite::Transaction<'_>,
        session: &str,
        key: &str,
    ) -> Result<()> {
        tx.prepare_cached("INSERT INTO task_start_receipts VALUES(?1,?2,?3,?4)")?
            .execute(params![session, key, self.digest.as_slice(), self.options])?;
        Ok(())
    }
}

impl Kernel {
    pub(crate) fn lookup_task_start(
        &self,
        request: &StartRequest<'_>,
        key: &str,
    ) -> Result<StartLookup> {
        let mut stmt = self.db.prepare_cached(
            "SELECT receipt.input_sha256,receipt.options,summary.data FROM task_requests r
             LEFT JOIN task_start_receipts receipt USING(session_id,request_key)
             LEFT JOIN task_summaries summary ON summary.task_id=r.task_id
             WHERE r.session_id=?1 AND r.request_key=?2",
        )?;
        let mut rows = stmt.query(params![request.session, key])?;
        let Some(row) = rows.next()? else {
            return Ok(StartLookup::Missing);
        };
        let digest = row.get_ref(0)?;
        if digest == rusqlite::types::ValueRef::Null {
            return Ok(StartLookup::Legacy);
        }
        let options_json = row.get_ref(1)?.as_str().map_err(rusqlite::Error::from)?;
        let (backend, options): (TaskBackend, ContextOptions) = serde_json::from_str(options_json)?;
        if backend != request.backend
            || options != request.options
            || digest.as_blob().map_err(rusqlite::Error::from)?
                != Sha256::digest(request.input.as_bytes()).as_slice()
        {
            return Err(request_conflict());
        }
        let summary_json = row.get_ref(2)?.as_str().map_err(rusqlite::Error::from)?;
        Ok(StartLookup::Existing(serde_json::from_str(summary_json)?))
    }
}
