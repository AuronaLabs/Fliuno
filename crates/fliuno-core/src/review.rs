use super::*;
use rusqlite::{OptionalExtension, Transaction, params};

pub(crate) fn migrate(db: &Connection) -> Result<()> {
    db.execute_batch(
        "BEGIN IMMEDIATE;
        CREATE TABLE IF NOT EXISTS task_reconciliations(
            task_id TEXT PRIMARY KEY REFERENCES tasks(id), data TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS artifact_revisions(
            artifact_id TEXT PRIMARY KEY REFERENCES artifacts(id), data TEXT NOT NULL
        );
        PRAGMA user_version=72; COMMIT;",
    )?;
    Ok(())
}

pub(crate) fn record(tx: &Transaction<'_>, task_id: &str, event: &EventKind) -> Result<()> {
    match event {
        EventKind::TaskOutcomeReconciled { reconciliation } => {
            tx.execute(
                "INSERT INTO task_reconciliations VALUES (?1,?2)",
                params![task_id, serde_json::to_string(reconciliation)?],
            )?;
        }
        EventKind::ArtifactRevisionLinked { revision } => {
            tx.execute(
                "INSERT INTO artifact_revisions VALUES (?1,?2)",
                params![
                    revision.current.artifact_id,
                    serde_json::to_string(revision)?
                ],
            )?;
        }
        _ => {}
    }
    Ok(())
}

impl Kernel {
    pub fn task_reconciliation(&self, task_id: &str) -> Result<Option<TaskReconciliation>> {
        self.get_task_summary(task_id)?;
        self.read_reconciliation(task_id)
    }

    pub(crate) fn read_reconciliation(&self, task_id: &str) -> Result<Option<TaskReconciliation>> {
        let data = self
            .db
            .prepare_cached("SELECT data FROM task_reconciliations WHERE task_id=?1")?
            .query_row([task_id], |row| row.get::<_, String>(0))
            .optional()?;
        data.map(|data| serde_json::from_str(&data).map_err(Into::into))
            .transpose()
    }

    pub fn reconcile_task(&mut self, p: TaskReconcileParams) -> Result<TaskReconciliation> {
        if !p.state.is_terminal()
            || p.operator.trim().is_empty()
            || p.operator.len() > 128
            || p.operator.chars().any(char::is_control)
            || p.evidence.trim().is_empty()
            || p.evidence.len() > 2048
        {
            return Err(Error::Invalid(
                "reconciliation requires a terminal state, operator and evidence".into(),
            ));
        }
        let task = self.get_task(&p.task_id)?;
        if let Some(existing) = self.read_reconciliation(&p.task_id)? {
            return if existing.review == p {
                Ok(existing)
            } else {
                Err(Error::Conflict(
                    "task already has a reconciled outcome".into(),
                ))
            };
        }
        let run = task
            .agent_run
            .as_ref()
            .filter(|run| run.finished && run.outcome_unknown)
            .ok_or_else(|| {
                Error::Conflict("task has no settled run with an unknown outcome".into())
            })?;
        if let Some(check_id) = &p.check_id {
            let event = self.get_remote_task_check(&task.id, check_id)?;
            if !matches!(event.data, EventKind::AgentRemoteTaskChecked {
                    run_id, outcome: RemoteTaskCheckOutcome::Observed { state }, ..
                } if run_id == run.id && state == p.state)
            {
                return Err(Error::Conflict(
                    "remote check does not verify the reconciled state".into(),
                ));
            }
        }
        if run
            .remote_task
            .as_ref()
            .is_some_and(|remote| remote.state.is_terminal() && remote.state != p.state)
        {
            return Err(Error::Conflict(
                "reconciliation conflicts with recorded remote evidence".into(),
            ));
        }
        let reconciliation = TaskReconciliation {
            run_id: run.id.clone(),
            recorded_at_ms: agent::now_ms()?,
            review: p,
        };
        self.commit(
            &task,
            &[EventKind::TaskOutcomeReconciled {
                reconciliation: reconciliation.clone(),
            }],
            &[],
        )?;
        Ok(reconciliation)
    }

    pub fn artifact_revision(
        &self,
        target: &ArtifactGetParams,
    ) -> Result<Option<ArtifactRevision>> {
        self.get_artifact_summary(&target.task_id, &target.artifact_id)?;
        let data = self
            .db
            .prepare_cached("SELECT data FROM artifact_revisions WHERE artifact_id=?1")?
            .query_row([&target.artifact_id], |row| row.get::<_, String>(0))
            .optional()?;
        data.map(|data| serde_json::from_str(&data).map_err(Into::into))
            .transpose()
    }

    pub fn link_artifact_revision(
        &mut self,
        p: ArtifactLinkRevisionParams,
    ) -> Result<ArtifactRevision> {
        let previous = self.get_artifact_summary(&p.previous.task_id, &p.previous.artifact_id)?;
        let current = self.get_artifact_summary(&p.current.task_id, &p.current.artifact_id)?;
        let catalog = |id: &str| {
            self.db.query_row(
                "SELECT sequence, session_id FROM artifact_catalog WHERE artifact_id=?1",
                [id],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)),
            )
        };
        let (previous_sequence, previous_session) = catalog(&p.previous.artifact_id)?;
        let (current_sequence, current_session) = catalog(&p.current.artifact_id)?;
        if previous_session != current_session
            || previous_sequence >= current_sequence
            || previous.binary
            || current.binary
            || previous.media_type != current.media_type
        {
            return Err(Error::Invalid(
                "choose an older text artifact of the same media type in this session".into(),
            ));
        }
        if let Some(existing) = self.artifact_revision(&p.current)? {
            return if existing.previous.task_id == p.previous.task_id
                && existing.previous.artifact_id == p.previous.artifact_id
            {
                Ok(existing)
            } else {
                Err(Error::Conflict(
                    "artifact already has a previous revision".into(),
                ))
            };
        }
        let previous_digest = self.digest_artifact(&p.previous.task_id, &p.previous.artifact_id)?;
        let current_digest = self.digest_artifact(&p.current.task_id, &p.current.artifact_id)?;
        let task = self.get_task(&p.current.task_id)?;
        let revision = ArtifactRevision {
            previous: p.previous,
            current: p.current,
            previous_digest,
            current_digest,
        };
        self.commit(
            &task,
            &[EventKind::ArtifactRevisionLinked {
                revision: revision.clone(),
            }],
            &[],
        )?;
        Ok(revision)
    }
}
