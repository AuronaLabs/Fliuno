//! Store compact recovery facts with each task transition and read older snapshots on demand.
use super::*;
use std::borrow::Cow;

/// Operation presence is sufficient; the payload is read only when recovering.
#[derive(serde::Serialize, serde::Deserialize)]
pub(crate) struct ResumeFacts<'a> {
    pub(crate) status: TaskStatus,
    pub(crate) model_steps: u32,
    pub(crate) max_model_steps: Option<u32>,
    pub(crate) backend: Cow<'a, TaskBackend>,
    pub(crate) backend_authority: Option<Cow<'a, str>>,
    pub(crate) agent_run: bool,
    pub(crate) remote_continuation: bool,
    pub(crate) in_flight_tool: bool,
    pub(crate) model_outcome_unknown: bool,
}

pub(super) fn migrate(db: &mut Connection) -> Result<()> {
    let tx = db.transaction()?;
    // Existing rows are loaded on demand until their next normal transition.
    tx.execute_batch(
        "CREATE TABLE IF NOT EXISTS task_resume_facts (
           task_id TEXT PRIMARY KEY REFERENCES tasks(id) ON DELETE CASCADE,
           data TEXT NOT NULL
         ) WITHOUT ROWID",
    )?;
    tx.pragma_update(None, "user_version", 48)?;
    tx.commit()?;
    Ok(())
}

pub(super) fn record(tx: &rusqlite::Transaction<'_>, task: &Task) -> Result<()> {
    let facts = serde_json::to_string(&ResumeFacts::from(task))?;
    tx.prepare_cached(
        "INSERT INTO task_resume_facts(task_id,data) VALUES(?1,?2)
         ON CONFLICT(task_id) DO UPDATE SET data=excluded.data
         WHERE task_resume_facts.data<>excluded.data",
    )?
    .execute(params![task.id, facts])?;
    Ok(())
}

// Keep conversion of legacy snapshots identical for point and batch reads.
macro_rules! resume_projection {
    () => {
        "task.data -> '$.status',
         json_extract(task.data,'$.modelSteps'),
         json_extract(task.data,'$.maxModelSteps'),
         task.data -> '$.backend',
         json_extract(task.data,'$.backendAuthority'),
         COALESCE(json_type(task.data,'$.agentRun')<>'null',0),
         COALESCE(json_type(task.data,'$.remoteContinuation')<>'null',0),
         COALESCE(json_type(task.data,'$.inFlightTool')<>'null',0),
         COALESCE(task.data -> '$.modelOutcomeUnknown','false')"
    };
}

impl Kernel {
    pub(crate) fn read_resume_facts(&self, task_id: &str) -> Result<ResumeFacts<'static>> {
        let mut statement = self.db.prepare_cached(concat!(
            "SELECT ",
            resume_projection!(),
            ", cached.data
             FROM (SELECT ?1 AS id) AS requested
             LEFT JOIN task_resume_facts AS cached ON cached.task_id=requested.id
             LEFT JOIN tasks AS task ON task.id=requested.id AND cached.task_id IS NULL
             WHERE cached.task_id IS NOT NULL OR task.id IS NOT NULL"
        ))?;
        let mut rows = statement.query([task_id])?;
        let row = rows.next()?.ok_or_else(|| Error::NotFound("task".into()))?;
        decode_facts(row)
    }

    pub(crate) fn read_resume_facts_batch(
        &self,
        request: &TaskSummaryBatchParams,
    ) -> Result<(Vec<Option<ResumeFacts<'static>>>, u64)> {
        self.validate_task_lookup_batch(request)?;
        // Filter by the indexed owner before opening the task snapshot.
        let mut statement = self.db.prepare_cached(concat!(
            "SELECT ",
            resume_projection!(),
            ", cached.data, requested.key,
             (cached.task_id IS NOT NULL OR task.id IS NOT NULL),
             (SELECT COALESCE(MAX(sequence),0) FROM events)
             FROM json_each(?1) AS requested
             LEFT JOIN task_summary_status AS owner ON owner.task_id=requested.value
                AND (?2 IS NULL OR owner.workspace_id=?2)
                AND (?3 IS NULL OR owner.session_id=?3)
             LEFT JOIN task_resume_facts AS cached ON cached.task_id=owner.task_id
             LEFT JOIN tasks AS task ON task.id=owner.task_id AND cached.task_id IS NULL"
        ))?;
        let ids = serde_json::to_string(&request.task_ids)?;
        let mut rows = statement.query(params![ids, request.workspace_id, request.session_id])?;
        let mut items = Vec::with_capacity(request.task_ids.len());
        items.resize_with(request.task_ids.len(), || None);
        let mut through_sequence = None;
        while let Some(row) = rows.next()? {
            if through_sequence.is_none() {
                through_sequence = Some(row.get::<_, i64>(12)? as u64);
            }
            if row.get::<_, bool>(11)? {
                let index = usize::from(row.get::<_, u16>(10)?);
                items[index] = Some(decode_facts(row)?);
            }
        }
        Ok((
            items,
            through_sequence.ok_or(rusqlite::Error::QueryReturnedNoRows)?,
        ))
    }
}

fn decode_facts(row: &rusqlite::Row<'_>) -> Result<ResumeFacts<'static>> {
    let json = |index| {
        row.get_ref(index)?
            .as_str_or_null()
            .map_err(rusqlite::Error::from)
    };
    if let Some(data) = json(9)? {
        return Ok(serde_json::from_str(data)?);
    }
    Ok(ResumeFacts {
        status: serde_json::from_str(json(0)?.unwrap_or("null"))?,
        model_steps: row.get(1)?,
        max_model_steps: row.get(2)?,
        // A missing field predates agent tasks. Explicit JSON null is invalid.
        backend: Cow::Owned(
            json(3)?
                .map(serde_json::from_str)
                .transpose()?
                .unwrap_or_default(),
        ),
        backend_authority: row.get::<_, Option<String>>(4)?.map(Cow::Owned),
        agent_run: row.get(5)?,
        remote_continuation: row.get(6)?,
        in_flight_tool: row.get(7)?,
        model_outcome_unknown: serde_json::from_str(json(8)?.unwrap_or("false"))?,
    })
}
