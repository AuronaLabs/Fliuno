//! Private MCP task contracts and pre-dispatch input receipts.
use super::*;
use rusqlite::{OpenFlags, TransactionBehavior};
use serde_json::Value;
use std::{sync::Mutex, time::Duration};

const MAX_BINDINGS: i64 = 4096;
const MAX_INPUTS: usize = 64;

pub struct McpTaskRecord {
    pub workspace_id: String,
    pub session_id: String,
    pub local_task_id: String,
    pub local_call_id: String,
    pub created_at: String,
    pub output_schema: Option<Value>,
}

pub struct McpTaskStore {
    pub(super) db: Mutex<Connection>,
    // An outstanding adapter worker must retain this database's process lease.
    _lock: Arc<File>,
}

impl Kernel {
    pub fn mcp_task_store(&self) -> Result<Option<Arc<McpTaskStore>>> {
        let Some(lock) = &self._lock else {
            return Ok(None);
        };
        let path = self
            .db
            .path()
            .ok_or_else(|| Error::Invalid("MCP database path unavailable".into()))?;
        let db = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_WRITE)?;
        db.busy_timeout(Duration::from_millis(100))?;
        db.execute_batch("PRAGMA foreign_keys=ON; PRAGMA synchronous=FULL;")?;
        Ok(Some(Arc::new(McpTaskStore {
            db: Mutex::new(db),
            _lock: Arc::clone(lock),
        })))
    }
}

impl McpTaskStore {
    pub fn insert(&self, server: &str, remote: &str, record: &McpTaskRecord) -> Result<bool> {
        let schema = record
            .output_schema
            .as_ref()
            .map(serde_json::to_string)
            .transpose()?;
        if schema.as_ref().is_some_and(|value| value.len() > 64 * 1024) {
            return Err(Error::Invalid(
                "MCP output schema exceeds storage limit".into(),
            ));
        }
        let db = self.db.lock().expect("MCP task store lock poisoned");
        Ok(db.execute(
            "INSERT INTO mcp_task_bindings
               SELECT ?1,?2,t.id,?4,?5,?6 FROM tasks t JOIN sessions s ON s.id=t.session_id
               WHERE t.id=?3 AND s.id=?7 AND s.workspace_id=?8
               AND (SELECT count(*) FROM mcp_task_bindings WHERE server_identity=?1)< ?9
               ON CONFLICT(server_identity,remote_id) DO NOTHING",
            params![
                server,
                remote,
                record.local_task_id,
                record.local_call_id,
                record.created_at,
                schema,
                record.session_id,
                record.workspace_id,
                MAX_BINDINGS
            ],
        )? == 1)
    }

    pub fn get(&self, server: &str, remote: &str) -> Result<Option<McpTaskRecord>> {
        let db = self.db.lock().expect("MCP task store lock poisoned");
        let row = db.query_row(
            "SELECT s.workspace_id,s.id,b.local_task_id,b.local_call_id,b.created_at,b.output_schema
             FROM mcp_task_bindings b JOIN tasks t ON t.id=b.local_task_id
             JOIN sessions s ON s.id=t.session_id WHERE b.server_identity=?1 AND b.remote_id=?2",
            params![server, remote],
            |row| Ok((McpTaskRecord {
                workspace_id: row.get(0)?, session_id: row.get(1)?, local_task_id: row.get(2)?,
                local_call_id: row.get(3)?, created_at: row.get(4)?, output_schema: None,
            }, row.get::<_, Option<String>>(5)?)),
        ).optional()?;
        let Some((mut record, schema)) = row else {
            return Ok(None);
        };
        record.output_schema = schema.as_deref().map(serde_json::from_str).transpose()?;
        Ok(Some(record))
    }

    /// Spend a batch together. A failed transport dispatch never frees this receipt.
    pub fn reserve_inputs<'a>(
        &self,
        server: &str,
        remote: &str,
        inputs: impl ExactSizeIterator<Item = &'a str>,
    ) -> Result<bool> {
        let mut db = self.db.lock().expect("MCP task store lock poisoned");
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let count = tx.query_row(
            "SELECT count(*) FROM mcp_task_input_receipts WHERE server_identity=?1 AND remote_id=?2",
            params![server, remote], |row| row.get::<_, u32>(0),
        )?;
        if (count as usize).saturating_add(inputs.len()) > MAX_INPUTS {
            return Ok(false);
        }
        for input in inputs {
            if tx.execute(
                "INSERT INTO mcp_task_input_receipts VALUES(?1,?2,?3) ON CONFLICT DO NOTHING",
                params![server, remote, input],
            )? != 1
            {
                return Ok(false);
            }
        }
        tx.commit()?;
        Ok(true)
    }
}
