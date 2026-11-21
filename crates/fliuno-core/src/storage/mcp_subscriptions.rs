//! Bounded local resource subscription intents. A committed observation receipt
//! survives the process, so interruption cannot masquerade as a clean close.
use super::*;
use rusqlite::TransactionBehavior;

const MAX_INTENTS: i64 = 4096;
const MAX_SESSION_INTENTS: i64 = 32;

pub struct McpSubscriptionKey<'a> {
    pub workspace_id: &'a str,
    pub session_id: &'a str,
    pub server_identity: &'a str,
    pub uri: &'a str,
}

pub struct McpSubscriptionAttempt {
    generation: i64,
    pub previous_snapshot: Option<String>,
    pub was_disconnected: bool,
}

pub(super) fn migrate(db: &Connection) -> Result<()> {
    db.execute_batch(
        "BEGIN IMMEDIATE;
         CREATE TABLE IF NOT EXISTS mcp_resource_subscriptions(
             session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
             server_identity TEXT NOT NULL,
             uri TEXT NOT NULL,
             generation INTEGER NOT NULL,
             watching INTEGER NOT NULL,
             snapshot TEXT,
             PRIMARY KEY(session_id,server_identity,uri)
         ) WITHOUT ROWID;
         PRAGMA user_version=64;
         COMMIT;",
    )?;
    Ok(())
}

impl McpTaskStore {
    pub fn begin_resource_subscription(
        &self,
        key: &McpSubscriptionKey<'_>,
    ) -> Result<McpSubscriptionAttempt> {
        let mut db = self.db.lock().expect("MCP store lock poisoned");
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let scope: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM sessions WHERE id=?1 AND workspace_id=?2)",
            params![key.session_id, key.workspace_id],
            |row| row.get(0),
        )?;
        if !scope {
            return Err(Error::NotFound("session".into()));
        }
        let id = params![key.session_id, key.server_identity, key.uri];
        let previous = tx
            .query_row(
                "SELECT generation,watching,snapshot FROM mcp_resource_subscriptions
             WHERE session_id=?1 AND server_identity=?2 AND uri=?3",
                id,
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, bool>(1)?,
                        row.get::<_, Option<String>>(2)?,
                    ))
                },
            )
            .optional()?;
        let (generation, was_disconnected, previous_snapshot) = match previous {
            Some((generation, watching, snapshot)) => {
                let generation = generation.checked_add(1).ok_or_else(|| {
                    Error::Conflict("MCP subscription generation exhausted".into())
                })?;
                tx.execute(
                    "UPDATE mcp_resource_subscriptions SET generation=?4,watching=1
                     WHERE session_id=?1 AND server_identity=?2 AND uri=?3",
                    params![key.session_id, key.server_identity, key.uri, generation],
                )?;
                (generation, watching, snapshot)
            }
            None => {
                let (total, session): (i64, i64) = tx.query_row(
                    "SELECT (SELECT count(*) FROM mcp_resource_subscriptions),
                        (SELECT count(*) FROM mcp_resource_subscriptions WHERE session_id=?1)",
                    [key.session_id],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )?;
                if total >= MAX_INTENTS || session >= MAX_SESSION_INTENTS {
                    return Err(Error::Conflict(
                        "MCP subscription intent limit reached".into(),
                    ));
                }
                tx.execute(
                    "INSERT INTO mcp_resource_subscriptions VALUES(?1,?2,?3,1,1,NULL)",
                    id,
                )?;
                (1, false, None)
            }
        };
        tx.commit()?;
        Ok(McpSubscriptionAttempt {
            generation,
            previous_snapshot,
            was_disconnected,
        })
    }

    pub fn finish_resource_subscription(
        &self,
        key: &McpSubscriptionKey<'_>,
        attempt: &McpSubscriptionAttempt,
        snapshot: Option<&str>,
    ) -> Result<()> {
        let db = self.db.lock().expect("MCP store lock poisoned");
        let changed = db.execute(
            "UPDATE mcp_resource_subscriptions SET watching=0,snapshot=COALESCE(?5,snapshot)
             WHERE session_id=?1 AND server_identity=?2 AND uri=?3 AND generation=?4 AND watching=1
             AND EXISTS(SELECT 1 FROM sessions WHERE id=?1 AND workspace_id=?6)",
            params![
                key.session_id,
                key.server_identity,
                key.uri,
                attempt.generation,
                snapshot,
                key.workspace_id
            ],
        )?;
        if changed != 1 {
            return Err(Error::Conflict(
                "MCP subscription observation was superseded".into(),
            ));
        }
        Ok(())
    }
}
