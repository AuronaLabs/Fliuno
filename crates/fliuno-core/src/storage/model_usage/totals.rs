//! Fixed-size sums maintained in the same transaction as indexed usage events.
use super::*;

use std::collections::{HashMap, hash_map::Entry};

fn owner(scope: &ModelUsageSummaryScope) -> (&'static str, &str, &'static str) {
    // Select from the owner table so a missing row differs from a present
    // owner with no aggregate. Both lookups use their existing primary keys.
    match scope {
        ModelUsageSummaryScope::Task { task_id } => (
            "task",
            task_id,
            "SELECT u.totals FROM tasks t LEFT JOIN model_usage_totals u
             ON u.scope=0 AND u.owner=t.id WHERE t.id=?1",
        ),
        ModelUsageSummaryScope::Session { session_id } => (
            "session",
            session_id,
            "SELECT u.totals FROM sessions s LEFT JOIN model_usage_totals u
             ON u.scope=1 AND u.owner=s.id WHERE s.id=?1",
        ),
        ModelUsageSummaryScope::Workspace { workspace_id } => (
            "workspace",
            workspace_id,
            "SELECT u.totals FROM workspaces w LEFT JOIN model_usage_totals u
             ON u.scope=2 AND u.owner=w.id WHERE w.id=?1",
        ),
    }
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
        accumulate(total, usage);
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

pub(super) fn accumulate(total: &mut ModelUsageTotals, usage: &ModelUsage) {
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

impl Kernel {
    pub fn model_usage_summary(
        &self,
        request: &ModelUsageSummaryParams,
    ) -> Result<ModelUsageSummary> {
        let (kind, id, sql) = owner(&request.scope);
        let tx = self.db.unchecked_transaction()?;
        let totals = read_totals(&tx, id, sql)?.ok_or_else(|| Error::NotFound(kind.into()))?;
        let through_sequence = self.event_watermark()?;
        tx.commit()?;
        Ok(ModelUsageSummary {
            through_sequence,
            totals,
        })
    }

    pub fn model_usage_summaries(
        &self,
        request: &ModelUsageSummaryBatchParams,
    ) -> Result<ModelUsageSummaryBatch> {
        let scopes = &request.scopes;
        if !(1..=MAX_MODEL_USAGE_SUMMARY_BATCH).contains(&scopes.len()) {
            return Err(Error::Invalid(format!(
                "scopes must contain 1 to {MAX_MODEL_USAGE_SUMMARY_BATCH} entries"
            )));
        }
        if scopes.iter().any(|scope| {
            let (_, id, _) = owner(scope);
            id.is_empty()
                || id.len() > MAX_MODEL_USAGE_OWNER_ID_BYTES
                || !id.bytes().all(|byte| byte.is_ascii_graphic())
        }) {
            return Err(Error::Invalid(format!(
                "owner IDs must be 1 to {MAX_MODEL_USAGE_OWNER_ID_BYTES} visible ASCII bytes"
            )));
        }
        // Pin all reads to the watermark view. Do one lookup per distinct
        // scope, but keep each requested position in the response.
        let tx = self.db.unchecked_transaction()?;
        let through_sequence = self.event_watermark()?;
        let mut seen = HashMap::with_capacity(scopes.len());
        let mut items = Vec::with_capacity(scopes.len());
        for scope in scopes {
            let totals = match seen.entry(scope) {
                Entry::Occupied(entry) => entry.into_mut(),
                Entry::Vacant(entry) => {
                    let (_, id, sql) = owner(scope);
                    entry.insert(read_totals(&tx, id, sql)?)
                }
            };
            items.push(totals.clone());
        }
        tx.commit()?;
        Ok(ModelUsageSummaryBatch {
            through_sequence,
            items,
        })
    }
}

fn read_totals(db: &Connection, id: &str, sql: &str) -> Result<Option<ModelUsageTotals>> {
    let mut query = db.prepare_cached(sql)?;
    let mut rows = query.query([id])?;
    let Some(row) = rows.next()? else {
        return Ok(None);
    };
    let totals = row
        .get_ref(0)?
        .as_str_or_null()
        .map_err(rusqlite::Error::from)?
        .map(serde_json::from_str)
        .transpose()?
        .unwrap_or_default();
    Ok(Some(totals))
}
