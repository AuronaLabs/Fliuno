//! Task invalidations share the event cursor but contain no bodies.
use super::*;
use fliuno_protocol::{TaskChange, TaskChangesPage, TaskChangesParams};
use std::collections::HashSet;

impl EventReplay {
    /// Page unique changed tasks in this window using the same durable event
    /// cursor. Choose this instead of `next_page` when only summaries are needed.
    pub fn next_task_changes<R: BufRead, W: Write>(
        &mut self,
        client: &mut Client<R, W>,
        limit: u32,
        on_notification: impl FnMut(Notification),
    ) -> Result<Option<Vec<TaskChange>>, Error> {
        if self.model_usage_only {
            return Err(Error::InvalidReplayWindow);
        }
        if self.after == self.through {
            return Ok(None);
        }
        let mut request = TaskChangesParams {
            task_id: None,
            session_id: None,
            workspace_id: None,
            after: self.after,
            through: Some(self.through),
            limit,
        };
        match &self.scope {
            ReplayScope::All => {}
            ReplayScope::Task(id) => request.task_id = Some(id.clone()),
            ReplayScope::Session(id) => request.session_id = Some(id.clone()),
            ReplayScope::Workspace(id) => request.workspace_id = Some(id.clone()),
        }
        let page: TaskChangesPage =
            client.call("events/listTaskChanges", &request, on_notification)?;
        if page.through_sequence != self.through || page.changes.len() > limit as usize {
            return Err(Error::InvalidReplayPage);
        }
        let mut last = self.after;
        let mut ids = HashSet::with_capacity(page.changes.len());
        for change in &page.changes {
            if change.sequence <= last
                || change.sequence > self.through
                || change.task_id.is_empty()
                || request
                    .task_id
                    .as_ref()
                    .is_some_and(|id| id != &change.task_id)
                || !ids.insert(change.task_id.as_str())
            {
                return Err(Error::InvalidReplayPage);
            }
            last = change.sequence;
        }
        if page.has_more {
            if last == self.after || last == self.through || page.next_after != last {
                return Err(Error::InvalidReplayPage);
            }
        } else if page.next_after != self.through {
            return Err(Error::InvalidReplayPage);
        }
        self.after = page.next_after;
        Ok((!page.changes.is_empty()).then_some(page.changes))
    }
}
