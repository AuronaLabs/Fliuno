//! Atomic durable cancellation of an explicitly scoped selection.
use super::*;
use crate::task::transition_to_cancelled;
use std::collections::HashSet;

impl Kernel {
    pub fn cancel_task_batch(
        &mut self,
        request: &TaskCancelBatchParams,
    ) -> Result<TaskCancelBatch> {
        let (scope, owner) = match &request.scope {
            TaskCancelScope::Session { session_id } => ("session_id", session_id),
            TaskCancelScope::Workspace { workspace_id } => ("workspace_id", workspace_id),
        };
        let valid_id = |id: &str| {
            (1..=MAX_TASK_CANCEL_ID_BYTES).contains(&id.len())
                && id.bytes().all(|byte| byte.is_ascii_graphic())
        };
        let mut seen = HashSet::with_capacity(request.task_ids.len().min(MAX_TASK_CANCEL_BATCH));
        if !(1..=MAX_TASK_CANCEL_BATCH).contains(&request.task_ids.len())
            || !valid_id(owner)
            || request
                .task_ids
                .iter()
                .any(|id| !valid_id(id) || !seen.insert(id))
        {
            return Err(Error::Invalid(format!(
                "select 1 to {MAX_TASK_CANCEL_BATCH} distinct tasks; IDs must be 1 to {MAX_TASK_CANCEL_ID_BYTES} visible ASCII bytes"
            )));
        }

        let tx = self.db.transaction()?;
        let mut items = Vec::with_capacity(request.task_ids.len());
        {
            // Check every ID from the narrow projection before loading the
            // first task body. Missing and out-of-scope IDs are indistinguishable.
            let sql = format!(
                "SELECT s.data FROM task_summary_status o
                JOIN task_summaries s ON s.task_id=o.task_id WHERE o.task_id=?1 AND o.{scope}=?2"
            );
            let mut read = tx.prepare_cached(&sql)?;
            for id in &request.task_ids {
                let mut rows = read.query(params![id, owner])?;
                let row = rows
                    .next()?
                    .ok_or_else(|| Error::NotFound("task in selected scope".into()))?;
                let summary = row.get_ref(0)?.as_str().map_err(rusqlite::Error::from)?;
                items.push(TaskCancelBatchItem {
                    task: serde_json::from_str(summary)?,
                    changed: false,
                });
            }
        }
        let mut committed = Vec::with_capacity(items.len());
        for (index, item) in items.iter_mut().enumerate() {
            if item.task.status.is_terminal() {
                continue;
            }
            // Keep only one full task in memory and hold lightweight publish
            // data until the whole batch commits.
            let mut task = read_task(&tx, &item.task.id)?;
            let events = transition_to_cancelled(&mut task);
            let sequences = write_transition(&tx, &task, &events, &[], None, None, None)?;
            item.task = task_summaries::summarize(&task);
            item.changed = true;
            committed.push((index, sequences, events));
        }
        let through_sequence = tx
            .prepare_cached("SELECT COALESCE(MAX(sequence),0) FROM events")?
            .query_row([], |row| row.get::<_, i64>(0))? as u64;
        tx.commit()?;
        for (index, sequences, events) in committed {
            let task = &items[index].task;
            self.publish_task_events(&task.id, &task.session_id, &sequences, &events);
            self.signal_agent_cancellation(&task.id);
        }
        Ok(TaskCancelBatch {
            items,
            through_sequence,
        })
    }
}
