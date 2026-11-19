//! Browse live, compact task summaries and preserve the query with its cursor.
use super::{Client, Error, Notification};
use fliuno_protocol::{
    ObjectPage, TaskSummary, TaskSummaryListParams, TaskSummaryOrder,
    WorkspaceTaskSummaryListParams,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashSet,
    io::{BufRead, Write},
};

/// Store this request together with the last consumed page. The cursor is
/// database-local and remains tied to the scope, status and order.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "scope", content = "params", rename_all = "snake_case")]
pub enum TaskSummaryCatalogRequest {
    Session(TaskSummaryListParams),
    Workspace(WorkspaceTaskSummaryListParams),
}

#[derive(Debug)]
pub struct TaskSummaryCatalog {
    next: Option<TaskSummaryCatalogRequest>,
}

impl TaskSummaryCatalog {
    /// Start at `after: 0` or restore a saved request for the same database.
    /// Check `sessionTaskSummaryOrdering` or `workspaceTaskSummaryOrdering`
    /// before choosing `NewestFirst`.
    pub fn new(mut request: TaskSummaryCatalogRequest) -> Self {
        let order = match &mut request {
            TaskSummaryCatalogRequest::Session(params) => &mut params.order,
            TaskSummaryCatalogRequest::Workspace(params) => &mut params.order,
        };
        if *order == Some(TaskSummaryOrder::OldestFirst) {
            *order = None;
        }
        Self {
            next: Some(request),
        }
    }

    /// Save only after consuming the preceding page. `None` means exhausted.
    pub fn next_request(&self) -> Option<&TaskSummaryCatalogRequest> {
        self.next.as_ref()
    }

    /// Query one page of current summaries. RPC errors and rejected pages leave
    /// the checkpoint unchanged. Pages do not freeze later status changes.
    pub fn next_page<R: BufRead, W: Write>(
        &mut self,
        client: &mut Client<R, W>,
        on_notification: impl FnMut(Notification),
    ) -> Result<Option<ObjectPage<TaskSummary>>, Error> {
        let Some(request) = &mut self.next else {
            return Ok(None);
        };
        let page = match request {
            TaskSummaryCatalogRequest::Session(params) => {
                client.call("task/listSummaries", params, on_notification)?
            }
            TaskSummaryCatalogRequest::Workspace(params) => {
                client.call("task/listWorkspaceSummaries", params, on_notification)?
            }
        };
        validate(request, &page)?;
        if page.has_more {
            match request {
                TaskSummaryCatalogRequest::Session(params) => params.after = page.next_after,
                TaskSummaryCatalogRequest::Workspace(params) => params.after = page.next_after,
            }
        } else {
            self.next = None;
        }
        Ok(Some(page))
    }
}

fn validate(
    request: &TaskSummaryCatalogRequest,
    page: &ObjectPage<TaskSummary>,
) -> Result<(), Error> {
    let (after, limit, order, target_bytes, status, session) = match request {
        TaskSummaryCatalogRequest::Session(p) => (
            p.after,
            p.limit,
            p.order,
            p.target_bytes,
            p.status,
            Some(p.session_id.as_str()),
        ),
        TaskSummaryCatalogRequest::Workspace(p) => {
            (p.after, p.limit, p.order, p.target_bytes, p.status, None)
        }
    };
    let count = page.items.len();
    let next = page.next_after;
    let cursor_valid = if count == 0 {
        next == after && !page.has_more
    } else {
        (1..=i64::MAX as u64).contains(&next)
            && match order.unwrap_or_default() {
                TaskSummaryOrder::OldestFirst => next > after,
                TaskSummaryOrder::NewestFirst => {
                    (after == 0 || next < after) && (!page.has_more || next > 1)
                }
            }
    };
    if !cursor_valid
        || count > limit as usize
        || (page.has_more && target_bytes.is_none() && count != limit as usize)
    {
        return Err(Error::InvalidTaskSummaryPage);
    }
    // A workspace summary lacks a workspace ID, so membership is checked
    // server-side. For session lists the owner is also visible on the wire.
    let mut ids = HashSet::with_capacity(count);
    if page.items.iter().any(|item| {
        item.id.is_empty()
            || item.session_id.is_empty()
            || session.is_some_and(|s| item.session_id != s)
            || status.is_some_and(|s| item.status != s)
            || !ids.insert(item.id.as_str())
    }) {
        return Err(Error::InvalidTaskSummaryPage);
    }
    Ok(())
}

#[cfg(test)]
mod tests;
