//! Fixed-size sums maintained in the same transaction as indexed usage events.
use super::*;

#[derive(Clone, Copy)]
#[repr(u8)]
enum Scope {
    Task = 0,
    Session = 1,
    Workspace = 2,
}

pub(in crate::storage) fn migrate_totals(db: &mut Connection) -> Result<()> {
    let tx = db.transaction()?;
    tx.execute_batch(
        "CREATE TABLE IF NOT EXISTS model_usage_totals (
             scope INTEGER NOT NULL CHECK(scope BETWEEN 0 AND 2),
             owner TEXT NOT NULL,
             totals TEXT NOT NULL,
             PRIMARY KEY(scope,owner)
         ) WITHOUT ROWID;
         DELETE FROM model_usage_totals;",
    )?;
    {
        // Stream only report rows, keeping a bounded amount of data in memory.
        let mut statement = tx.prepare(
            "SELECT u.task_id,u.session_id,u.workspace_id,e.data FROM model_usage u
             CROSS JOIN events e ON e.sequence=u.sequence",
        )?;
        let mut rows = statement.query([])?;
        while let Some(row) = rows.next()? {
            let EventKind::ModelUsageReported { usage, .. } =
                serde_json::from_str(row.get_ref(3)?.as_str().map_err(rusqlite::Error::from)?)?
            else {
                return Err(rusqlite::Error::InvalidQuery.into());
            };
            record(
                &tx,
                [
                    &row.get::<_, String>(0)?,
                    &row.get::<_, String>(1)?,
                    &row.get::<_, String>(2)?,
                ],
                &usage,
            )?;
        }
    }
    tx.pragma_update(None, "user_version", 43)?;
    tx.commit()?;
    Ok(())
}

pub(super) fn record(tx: &Transaction<'_>, owners: [&str; 3], usage: &ModelUsage) -> Result<()> {
    // One SQLite event sequence (signed 64-bit) contributes at most one u64
    // per field, so u128 can represent every valid database's exact totals.
    let mut totals: [ModelUsageTotals; 3] = std::array::from_fn(|_| ModelUsageTotals::default());
    let mut select = tx.prepare_cached(
        "SELECT scope,totals FROM model_usage_totals WHERE scope=0 AND owner=?1
         UNION ALL SELECT scope,totals FROM model_usage_totals WHERE scope=1 AND owner=?2
         UNION ALL SELECT scope,totals FROM model_usage_totals WHERE scope=2 AND owner=?3",
    )?;
    let mut rows = select.query(owners)?;
    while let Some(row) = rows.next()? {
        let scope = usize::from(row.get::<_, u8>(0)?);
        totals[scope] =
            serde_json::from_str(row.get_ref(1)?.as_str().map_err(rusqlite::Error::from)?)?;
    }
    drop(rows);
    for total in &mut totals {
        total.report_count.0 += 1;
        total.input_tokens.0 += u128::from(usage.input_tokens);
        total.output_tokens.0 += u128::from(usage.output_tokens);
        if let Some(count) = usage.cached_input_tokens {
            total.cached_input_report_count.0 += 1;
            total.cached_input_tokens.0 += u128::from(count);
        }
        if let Some(count) = usage.reasoning_output_tokens {
            total.reasoning_output_report_count.0 += 1;
            total.reasoning_output_tokens.0 += u128::from(count);
        }
    }
    let [task, session, workspace] = totals.map(|total| serde_json::to_string(&total));
    tx.prepare_cached(
        "INSERT INTO model_usage_totals(scope,owner,totals)
         VALUES (0,?1,?4),(1,?2,?5),(2,?3,?6)
         ON CONFLICT(scope,owner) DO UPDATE SET totals=excluded.totals",
    )?
    .execute(params![
        owners[0], owners[1], owners[2], task?, session?, workspace?
    ])?;
    Ok(())
}

impl Kernel {
    pub fn model_usage_summary(
        &self,
        request: &ModelUsageSummaryParams,
    ) -> Result<ModelUsageSummary> {
        let (scope, owner) = match &request.scope {
            ModelUsageSummaryScope::Task { task_id } => {
                self.require_task(task_id)?;
                (Scope::Task, task_id)
            }
            ModelUsageSummaryScope::Session { session_id } => {
                self.require_session(session_id)?;
                (Scope::Session, session_id)
            }
            ModelUsageSummaryScope::Workspace { workspace_id } => {
                self.require_workspace(workspace_id)?;
                (Scope::Workspace, workspace_id)
            }
        };
        let mut query = self.db.prepare_cached(
            "SELECT (SELECT COALESCE(MAX(sequence),0) FROM events),
                    (SELECT totals FROM model_usage_totals WHERE scope=?1 AND owner=?2)",
        )?;
        let mut rows = query.query(params![scope as u8, owner])?;
        let row = rows.next()?.ok_or(rusqlite::Error::QueryReturnedNoRows)?;
        let data = row
            .get_ref(1)?
            .as_str_or_null()
            .map_err(rusqlite::Error::from)?;
        Ok(ModelUsageSummary {
            through_sequence: row.get::<_, i64>(0)? as u64,
            totals: data
                .map(serde_json::from_str)
                .transpose()?
                .unwrap_or_default(),
        })
    }
}
