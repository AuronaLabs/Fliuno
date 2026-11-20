//! Project the recovery facts without sending task bodies through Serde.
use super::*;
use std::borrow::Cow;

/// Operation presence is sufficient; the payload is read only when recovering.
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

impl Kernel {
    pub(crate) fn read_resume_facts(&self, task_id: &str) -> Result<ResumeFacts<'static>> {
        let mut statement = self.db.prepare_cached(
            "SELECT data -> '$.status',
                json_extract(data,'$.modelSteps'),
                json_extract(data,'$.maxModelSteps'),
                data -> '$.backend',
                json_extract(data,'$.backendAuthority'),
                COALESCE(json_type(data,'$.agentRun')<>'null',0),
                COALESCE(json_type(data,'$.remoteContinuation')<>'null',0),
                COALESCE(json_type(data,'$.inFlightTool')<>'null',0),
                COALESCE(data -> '$.modelOutcomeUnknown','false')
             FROM tasks WHERE id=?1",
        )?;
        let mut rows = statement.query([task_id])?;
        let row = rows.next()?.ok_or_else(|| Error::NotFound("task".into()))?;
        let json = |index| {
            row.get_ref(index)?
                .as_str_or_null()
                .map_err(rusqlite::Error::from)
        };
        Ok(ResumeFacts {
            status: serde_json::from_str(json(0)?.unwrap_or("null"))?,
            model_steps: row.get(1)?,
            max_model_steps: row.get(2)?,
            // A missing field predates agent tasks; explicit JSON null is invalid.
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
}
