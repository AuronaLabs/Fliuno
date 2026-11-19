//! Paginate the live session catalog without changing a saved scope or ordering.
use super::{Client, Error, Notification};
use fliuno_protocol::{ObjectPage, Session, SessionListOrder, SessionListParams};
use std::{
    collections::HashSet,
    io::{BufRead, Write},
};

#[derive(Debug)]
pub struct SessionCatalog {
    request: SessionListParams,
    exhausted: bool,
}

impl SessionCatalog {
    /// Start at `after: 0` or restore a previously saved continuation request.
    /// Check `sessionListOrdering` before selecting `NewestFirst`.
    pub fn new(mut request: SessionListParams) -> Self {
        if request.order == Some(SessionListOrder::OldestFirst) {
            // Omit the new field so default browsing still works on older servers.
            request.order = None;
        }
        Self {
            request,
            exhausted: false,
        }
    }

    /// Save this request after consuming the preceding page. `None` means exhausted.
    pub fn next_request(&self) -> Option<&SessionListParams> {
        (!self.exhausted).then_some(&self.request)
    }

    pub fn next_page<R: BufRead, W: Write>(
        &mut self,
        client: &mut Client<R, W>,
        on_notification: impl FnMut(Notification),
    ) -> Result<Option<ObjectPage<Session>>, Error> {
        if self.exhausted {
            return Ok(None);
        }
        let page = client.call("session/list", &self.request, on_notification)?;
        validate(&self.request, &page)?;
        self.request.after = page.next_after;
        self.exhausted = !page.has_more;
        Ok(Some(page))
    }
}

fn validate(request: &SessionListParams, page: &ObjectPage<Session>) -> Result<(), Error> {
    let count = page.items.len();
    let after = request.after;
    let next = page.next_after;
    let cursor_valid = if count == 0 {
        next == after && !page.has_more
    } else {
        (1..=i64::MAX as u64).contains(&next)
            && match request.order.unwrap_or_default() {
                SessionListOrder::OldestFirst => next > after,
                SessionListOrder::NewestFirst => {
                    (after == 0 || next < after) && (!page.has_more || next > 1)
                }
            }
    };
    if !cursor_valid
        || count > request.limit as usize
        || (page.has_more && count != request.limit as usize)
    {
        return Err(Error::InvalidSessionCatalogPage);
    }
    let mut ids = HashSet::with_capacity(count);
    for item in &page.items {
        if item.workspace_id != request.workspace_id
            || request
                .archived
                .is_some_and(|archived| item.archived != archived)
            || item.id.is_empty()
            || !ids.insert(item.id.as_str())
        {
            return Err(Error::InvalidSessionCatalogPage);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
