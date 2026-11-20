//! Task invalidations share the event cursor but contain no event bodies.
use super::*;
use fliuno_protocol::{
    TaskChange, TaskChangesPage, TaskChangesParams, TaskSummaryChangesPage,
    TaskSummaryChangesParams,
};
use std::collections::HashSet;

impl EventReplay {
    /// Page unique changed tasks in this window using the same durable event
    /// cursor. Choose this instead of `next_page` when only IDs are needed.
    pub fn next_task_changes<R: BufRead, W: Write>(
        &mut self,
        client: &mut Client<R, W>,
        limit: u32,
        on_notification: impl FnMut(Notification),
    ) -> Result<Option<Vec<TaskChange>>, Error> {
        let Some(request) = self.task_changes_request(limit)? else {
            return Ok(None);
        };
        let page: TaskChangesPage =
            client.call("events/listTaskChanges", &request, on_notification)?;
        self.advance_task_changes(
            page.through_sequence,
            page.next_after,
            page.has_more,
            limit,
            false,
            page.changes
                .iter()
                .map(|c| (c.task_id.as_str(), c.sequence)),
        )?;
        Ok((!page.changes.is_empty()).then_some(page.changes))
    }

    /// Read current summaries for changed tasks with one RPC. Use when the
    /// server advertises `taskSummaryChangeReplay`. The summary watermark may
    /// be newer than this fixed event window; only `next_after` moves the cursor.
    pub fn next_task_summary_changes<R: BufRead, W: Write>(
        &mut self,
        client: &mut Client<R, W>,
        limit: u32,
        on_notification: impl FnMut(Notification),
    ) -> Result<Option<TaskSummaryChangesPage>, Error> {
        self.read_task_summary_changes(
            client,
            "events/listTaskSummaryChanges",
            limit,
            on_notification,
        )
    }

    /// Read only actual summary revisions recorded after the index was enabled.
    /// Older windows are conservatively replayed from the event log. Check
    /// `taskSummaryRevisionReplay` before selecting this reader.
    pub fn next_task_summary_revisions<R: BufRead, W: Write>(
        &mut self,
        client: &mut Client<R, W>,
        limit: u32,
        on_notification: impl FnMut(Notification),
    ) -> Result<Option<TaskSummaryChangesPage>, Error> {
        self.read_task_summary_changes(
            client,
            "events/listTaskSummaryRevisions",
            limit,
            on_notification,
        )
    }

    /// Page tasks at their latest revision in this fixed event window.
    /// An empty continuation page still advances the scanned cursor.
    pub fn next_latest_task_summary_revisions<R: BufRead, W: Write>(
        &mut self,
        client: &mut Client<R, W>,
        limit: u32,
        on_notification: impl FnMut(Notification),
    ) -> Result<Option<TaskSummaryChangesPage>, Error> {
        self.read_task_summary_changes(
            client,
            "events/listLatestTaskSummaryRevisions",
            limit,
            on_notification,
        )
    }

    fn read_task_summary_changes<R: BufRead, W: Write>(
        &mut self,
        client: &mut Client<R, W>,
        method: &str,
        limit: u32,
        on_notification: impl FnMut(Notification),
    ) -> Result<Option<TaskSummaryChangesPage>, Error> {
        let Some(request) = self.task_changes_request(limit)? else {
            return Ok(None);
        };
        let page: TaskSummaryChangesPage = client.call(
            method,
            &TaskSummaryChangesParams {
                task_id: request.task_id,
                session_id: request.session_id,
                workspace_id: request.workspace_id,
                after: request.after,
                through: request.through,
                limit,
            },
            on_notification,
        )?;
        if !(self.through..=i64::MAX as u64).contains(&page.summary_through_sequence)
            || matches!(&self.scope, ReplayScope::Session(id) if page.changes.iter().any(|c| &c.summary.session_id != id))
        {
            return Err(Error::InvalidReplayPage);
        }
        let scanned_cursor = method == "events/listLatestTaskSummaryRevisions";
        self.advance_task_changes(
            page.through_sequence,
            page.next_after,
            page.has_more,
            limit,
            scanned_cursor,
            page.changes
                .iter()
                .map(|c| (c.summary.id.as_str(), c.sequence)),
        )?;
        Ok((!page.changes.is_empty() || (scanned_cursor && page.has_more)).then_some(page))
    }

    fn task_changes_request(&self, limit: u32) -> Result<Option<TaskChangesParams>, Error> {
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
        Ok(Some(request))
    }

    fn advance_task_changes<'a>(
        &mut self,
        through: u64,
        next_after: u64,
        has_more: bool,
        limit: u32,
        scanned_cursor: bool,
        changes: impl ExactSizeIterator<Item = (&'a str, u64)>,
    ) -> Result<(), Error> {
        if through != self.through || changes.len() > limit as usize {
            return Err(Error::InvalidReplayPage);
        }
        let mut last = self.after;
        let mut ids = HashSet::with_capacity(changes.len());
        for (id, sequence) in changes {
            if sequence <= last
                || sequence > through
                || id.is_empty()
                || matches!(&self.scope, ReplayScope::Task(task_id) if task_id != id)
                || !ids.insert(id)
            {
                return Err(Error::InvalidReplayPage);
            }
            last = sequence;
        }
        if has_more {
            if !(self.after < next_after && next_after < through)
                || last > next_after
                || (!scanned_cursor && next_after != last)
            {
                return Err(Error::InvalidReplayPage);
            }
        } else if next_after != through {
            return Err(Error::InvalidReplayPage);
        }
        self.after = next_after;
        Ok(())
    }
}
