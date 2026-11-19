//! Correlate remote observations without paging through unrelated task events.
use super::*;

pub(super) fn migrate(db: &mut Connection) -> Result<()> {
    let tx = db.transaction()?;
    tx.execute_batch(
        "CREATE TABLE IF NOT EXISTS remote_task_checks(
            task_id TEXT NOT NULL REFERENCES tasks(id),
            check_id TEXT NOT NULL,
            sequence INTEGER NOT NULL REFERENCES events(sequence),
            PRIMARY KEY(task_id,check_id)
        ) WITHOUT ROWID;
        INSERT INTO remote_task_checks(task_id,check_id,sequence)
        SELECT task_id,json_extract(data,'$.checkId'),sequence FROM events
        WHERE json_extract(data,'$.type')='agent_remote_task_checked'
        ON CONFLICT(task_id,check_id) DO NOTHING;
        PRAGMA user_version=29;",
    )?;
    tx.commit()?;
    Ok(())
}

impl Kernel {
    pub fn get_remote_task_check(&self, task_id: &str, check_id: &str) -> Result<Event> {
        if check_id.is_empty()
            || check_id.len() > 128
            || !check_id
                .bytes()
                .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, b'_' | b'-'))
        {
            return Err(Error::Invalid("invalid remote check ID".into()));
        }
        let mut statement = self.db.prepare_cached(
            "SELECT events.sequence,events.task_id,events.data
             FROM remote_task_checks JOIN events USING(sequence)
             WHERE remote_task_checks.task_id=?1 AND check_id=?2",
        )?;
        let mut rows = statement.query(params![task_id, check_id])?;
        let row = rows
            .next()?
            .ok_or_else(|| Error::NotFound("remote task check".into()))?;
        replay::event_row(row)
    }

    /// Persist evidence and its lookup key in one commit before notifying clients.
    pub(crate) fn append_remote_task_check(
        &mut self,
        task_id: &str,
        run_id: String,
        check_id: String,
        outcome: RemoteTaskCheckOutcome,
    ) -> Result<()> {
        let tx = self.db.transaction()?;
        let session_id: String = tx
            .prepare_cached("SELECT session_id FROM tasks WHERE id=?1")?
            .query_row([task_id], |row| row.get(0))?;
        let data = EventKind::AgentRemoteTaskChecked {
            run_id,
            check_id: check_id.clone(),
            outcome,
        };
        tx.prepare_cached(INSERT_EVENT)?
            .execute(params![task_id, serde_json::to_string(&data)?])?;
        let sequence = tx.last_insert_rowid();
        tx.prepare_cached(
            "INSERT INTO remote_task_checks(task_id,check_id,sequence) VALUES(?1,?2,?3)",
        )?
        .execute(params![task_id, check_id, sequence])?;
        tx.commit()?;
        self.publish_task_events(task_id, &session_id, &[sequence as u64], &[data]);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn check_index_rejection_rolls_back_event_and_notification() {
        let mut kernel = Kernel::in_memory(Box::new(DemoProvider)).unwrap();
        let path = std::env::current_dir().unwrap();
        let workspace = kernel.open_workspace(path.to_str().unwrap()).unwrap();
        let session = kernel.create_session(&workspace.id).unwrap();
        let (task, _) = kernel
            .create_task_with_key(&session.id, "fixture", None)
            .unwrap();
        let updates = kernel.subscribe_events().unwrap();
        let check_id = "remote_check_fixture";
        let append = |kernel: &mut Kernel| {
            kernel.append_remote_task_check(
                &task.id,
                "fixture-run".into(),
                check_id.into(),
                RemoteTaskCheckOutcome::Unavailable,
            )
        };
        append(&mut kernel).unwrap();
        let committed = kernel.get_remote_task_check(&task.id, check_id).unwrap();
        assert_eq!(updates.try_recv().unwrap().sequence, committed.sequence);
        assert!(append(&mut kernel).is_err());
        assert_eq!(kernel.event_watermark().unwrap(), committed.sequence);
        assert!(updates.try_recv().is_err());
        assert_eq!(
            kernel
                .get_remote_task_check(&task.id, check_id)
                .unwrap()
                .sequence,
            committed.sequence
        );
    }
}
