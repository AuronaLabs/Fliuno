use super::*;

pub(super) fn sql_timestamp(ms: u64) -> Result<i64> {
    i64::try_from(ms).map_err(|_| Error::Conflict("run time exceeds the storage limit".into()))
}

pub(super) fn is_due(run: &Run) -> Result<bool> {
    match run.deadline_at_unix_ms {
        Some(deadline) => Ok(now_ms()? >= deadline),
        None => Ok(false),
    }
}

impl Kernel {
    /// Time until the next active deadline. An empty index can sleep until a new Run is submitted.
    pub fn next_run_deadline_delay_ms(&self) -> Result<Option<u64>> {
        let deadline: Option<i64> = self.db.query_row(
            "SELECT MIN(deadline_at_unix_ms) FROM run_deadlines",
            [],
            |row| row.get(0),
        )?;
        deadline
            .map(|deadline| Ok(deadline.saturating_sub(sql_timestamp(now_ms()?)?).max(0) as u64))
            .transpose()
    }

    /// Reconcile one overdue Run and return the task IDs whose cancellation committed.
    pub fn expire_next_run(&mut self) -> Result<Option<Vec<String>>> {
        let id: Option<String> = self
            .db
            .query_row(
                "SELECT run_id FROM run_deadlines WHERE deadline_at_unix_ms<=?1
             ORDER BY deadline_at_unix_ms, run_id LIMIT 1",
                [sql_timestamp(now_ms()?)?],
                |row| row.get(0),
            )
            .optional()?;
        match id {
            Some(id) => self.expire_run(&id),
            None => Ok(None),
        }
    }

    pub fn expire_run(&mut self, id: &str) -> Result<Option<Vec<String>>> {
        let mut state = self.read_run_state(id)?;
        if state.run.status.is_terminal() || !is_due(&state.run)? {
            return Ok(None);
        }
        Ok(Some(self.close_run(&mut state, true)?.1))
    }

    pub fn expire_run_for_task(&mut self, task_id: &str) -> Result<Option<Vec<String>>> {
        match self.expired_run_for_task(task_id)? {
            Some(id) => self.expire_run(&id),
            None => Ok(None),
        }
    }

    pub(crate) fn require_run_child_before_deadline(&self, task_id: &str) -> Result<()> {
        if self.expired_run_for_task(task_id)?.is_some() {
            return Err(Error::Conflict("parent run deadline elapsed".into()));
        }
        Ok(())
    }

    fn expired_run_for_task(&self, task_id: &str) -> Result<Option<String>> {
        let deadline: Option<(String, i64)> = self
            .db
            .prepare_cached(
                "SELECT d.run_id, d.deadline_at_unix_ms FROM run_nodes n
             JOIN run_deadlines d ON d.run_id=n.run_id WHERE n.task_id=?1",
            )?
            .query_row([task_id], |row| Ok((row.get(0)?, row.get(1)?)))
            .optional()?;
        match deadline {
            Some((id, deadline)) if sql_timestamp(now_ms()?)? >= deadline => Ok(Some(id)),
            _ => Ok(None),
        }
    }
}
