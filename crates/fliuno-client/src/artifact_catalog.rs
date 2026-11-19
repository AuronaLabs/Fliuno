//! Checked artifact catalog windows and an adjacent durable event feed.
use super::{Client, Error, EventFeed, EventReplay, Notification, ReplayScope};
use fliuno_protocol::{
    ArtifactCatalogParams, ArtifactCatalogScope, ArtifactCatalogSnapshotPage,
    ArtifactCatalogSnapshotParams, ArtifactSubscriptionSnapshot,
};
use std::{
    collections::HashSet,
    io::{BufRead, Write},
};

/// A fixed catalog window. Its cursor is never an event sequence.
#[derive(Debug)]
pub struct ArtifactCatalog {
    request: ArtifactCatalogSnapshotParams,
    exhausted: bool,
}

impl ArtifactCatalog {
    pub fn new(request: ArtifactCatalogSnapshotParams) -> Self {
        Self {
            request,
            exhausted: false,
        }
    }

    /// Persist after consuming the last returned page to resume a pinned window.
    pub fn next_request(&self) -> Option<&ArtifactCatalogSnapshotParams> {
        (!self.exhausted).then_some(&self.request)
    }

    /// The watermark to save once the entire window has been consumed, even
    /// if the last matching item precedes the end of the global catalog.
    pub fn completed_through(&self) -> Option<u64> {
        self.request.through_sequence.filter(|_| self.exhausted)
    }

    /// Start a new window after the exhausted one. Returns false if the
    /// current window still has unread pages.
    pub fn begin_next_window(&mut self) -> bool {
        let Some(through) = self.completed_through() else {
            return false;
        };
        self.request.after = through;
        self.request.through_sequence = None;
        self.exhausted = false;
        true
    }

    pub fn next_page<R: BufRead, W: Write>(
        &mut self,
        client: &mut Client<R, W>,
        on_notification: impl FnMut(Notification),
    ) -> Result<Option<ArtifactCatalogSnapshotPage>, Error> {
        let Some(request) = self.next_request() else {
            return Ok(None);
        };
        let page = client.call("artifacts/listSnapshot", request, on_notification)?;
        self.accept_page(&page)?;
        Ok(Some(page))
    }

    fn accept_page(&mut self, page: &ArtifactCatalogSnapshotPage) -> Result<(), Error> {
        let request = &self.request;
        let through = page.through_sequence;
        let count = page.items.len();
        if through > i64::MAX as u64
            || request
                .through_sequence
                .is_some_and(|expected| expected != through)
            || page.next_after > through
            || count > request.limit as usize
            || page
                .next_after
                .checked_sub(request.after)
                .is_none_or(|distance| distance < count as u64)
            || (count == 0 && page.next_after != request.after)
            || (page.has_more
                && (count == 0 || count != request.limit as usize || page.next_after == through))
        {
            return Err(Error::InvalidArtifactCatalogPage);
        }
        let mut ids = HashSet::with_capacity(count);
        if page.items.iter().any(|item| {
            item.id.is_empty()
                || item.task_id.is_empty()
                || !ids.insert(item.id.as_str())
                || request
                    .media_type
                    .as_ref()
                    .is_some_and(|media| media != &item.media_type)
        }) {
            return Err(Error::InvalidArtifactCatalogPage);
        }
        self.request.after = page.next_after;
        self.request.through_sequence = Some(through);
        self.exhausted = !page.has_more;
        Ok(())
    }
}

/// First catalog page, remaining pages, and post-snapshot events. Preserve both
/// cursor spaces independently. The media type applies only to catalog pages.
pub struct ArtifactSubscription {
    pub snapshot: ArtifactSubscriptionSnapshot,
    pub catalog: ArtifactCatalog,
    pub events: EventFeed,
}

impl<R: BufRead, W: Write> Client<R, W> {
    /// Open a current catalog window and replace the live event subscription in
    /// one RPC. Notifications before the reply still belong to the old stream.
    pub fn subscribe_artifacts(
        &mut self,
        request: ArtifactCatalogParams,
        on_notification: impl FnMut(Notification),
    ) -> Result<ArtifactSubscription, Error> {
        let snapshot: ArtifactSubscriptionSnapshot =
            self.call("artifacts/subscribe", &request, on_notification)?;
        let scope = match &request.scope {
            ArtifactCatalogScope::Session { session_id } => {
                ReplayScope::Session(session_id.clone())
            }
            ArtifactCatalogScope::Workspace { workspace_id } => {
                ReplayScope::Workspace(workspace_id.clone())
            }
        };
        let mut catalog = ArtifactCatalog::new(request.into());
        catalog.accept_page(&snapshot.catalog)?;
        let through = snapshot.event_through_sequence;
        let events = EventFeed::from_replay(EventReplay::new(scope, through, through)?);
        Ok(ArtifactSubscription {
            snapshot,
            catalog,
            events,
        })
    }
}
