//! Blocking stdio client for Fliuno's local application protocol.
pub mod ag_ui;
mod approval_decisions;
mod approval_details;
mod approval_inbox;
mod artifact_catalog;
mod download;
mod feed;
mod history;
mod history_search;
mod replay;
mod session_catalog;
mod task_cancel;
mod task_catalog;
mod task_resume;
mod task_summaries;
mod task_tool_input;
mod terminal;
mod wire;
mod work;

pub use approval_inbox::ApprovalInbox;
pub use artifact_catalog::{ArtifactCatalog, ArtifactSubscription};
pub use feed::EventFeed;
pub use history::{SessionHistory, SessionHistorySubscription};
pub use history_search::{HistorySearch, HistorySearchRequest};
pub use replay::{EventReplay, ModelUsageScope, ReplayScope};
pub use session_catalog::SessionCatalog;
pub use task_catalog::{TaskSummaryCatalog, TaskSummaryCatalogRequest};
pub use terminal::{TerminalFeed, TerminalFeedEvent, TerminalOutputPage};
pub use work::WorkSubscription;

use fliuno_protocol::{InitializeParams, PROTOCOL_VERSION};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;
use std::io::{self, BufRead, Write};

pub const DEFAULT_RESPONSE_BYTES: usize = 16 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("application connection is closed")]
    Closed,
    #[error("application transport: {0}")]
    Io(#[from] io::Error),
    #[error("could not encode the request")]
    RequestEncoding,
    #[error("request frame exceeds the server's byte limit")]
    RequestTooLarge,
    #[error("response frame exceeds the client's byte limit")]
    ResponseTooLarge,
    #[error("invalid application frame: {0}")]
    InvalidFrame(&'static str),
    #[error("application result has the wrong type")]
    InvalidResult,
    #[error("invalid event replay window")]
    InvalidReplayWindow,
    #[error("event page violates the requested replay window or scope")]
    InvalidReplayPage,
    #[error("invalid conversation history page")]
    InvalidHistoryPage,
    #[error("server does not support a lower bound for session history")]
    UnsupportedSessionHistoryAfterSequence,
    #[error("server does not support session history subscriptions")]
    UnsupportedSessionHistorySubscription,
    #[error("server does not support a lower bound for history search")]
    UnsupportedHistorySearchAfterSequence,
    #[error(
        "history scanLimit must be 1 to {0}",
        fliuno_protocol::MAX_HISTORY_SEARCH_SCAN_LIMIT
    )]
    InvalidHistorySearchScanLimit,
    #[error("server does not support the requested history scanLimit")]
    UnsupportedHistorySearchScanLimit,
    #[error("session catalog page violates its requested scope, filter or cursor order")]
    InvalidSessionCatalogPage,
    #[error("task summary page violates its requested scope, status or cursor order")]
    InvalidTaskSummaryPage,
    #[error("invalid approval inbox snapshot page")]
    InvalidApprovalInboxPage,
    #[error(
        "recent task limit must be 1 to {0}",
        fliuno_protocol::MAX_WORK_RECENT_TASKS
    )]
    InvalidRecentTaskLimit,
    #[error("work snapshot preview violates its requested scope or limit")]
    InvalidWorkPreview,
    #[error("invalid artifact catalog page")]
    InvalidArtifactCatalogPage,
    #[error("invalid expected artifact SHA-256")]
    InvalidArtifactDigest,
    #[error("artifact chunk violates the requested range or content length")]
    InvalidArtifactChunk,
    #[error("artifact SHA-256 does not match the expected digest")]
    ArtifactDigestMismatch,
    #[error("artifact staging data is longer than the expected body")]
    InvalidArtifactPrefix,
    #[error("reading artifact staging data: {0}")]
    ArtifactStagingRead(#[source] io::Error),
    #[error("recording the artifact download digest: {0}")]
    ArtifactCheckpoint(#[source] io::Error),
    #[error("writing artifact output: {0}")]
    ArtifactOutput(#[source] io::Error),
    #[error("invalid terminal output limit or byte cursor")]
    InvalidTerminalCursor,
    #[error("terminal output violates the requested identity, byte range or budget")]
    InvalidTerminalOutput,
    #[error("invalid terminal subscription identity or state")]
    InvalidTerminalSubscription,
    #[error("application RPC error {code}: {message}")]
    Rpc { code: i32, message: String },
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerInfo {
    pub protocol_version: String,
    pub server_name: String,
    pub server_version: String,
    pub capabilities: serde_json::Map<String, Value>,
}

#[derive(Debug, Deserialize)]
pub struct Notification {
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

/// The caller owns the pipes and the server's process lifetime. Keep one pending
/// request per connection and deliver notifications to a callback as they arrive.
pub struct Client<R, W> {
    reader: R,
    writer: W,
    info: ServerInfo,
    last_id: u64,
    usable: bool,
    frame: Vec<u8>,
}

impl<R: BufRead, W: Write> Client<R, W> {
    pub fn connect(
        mut reader: R,
        mut writer: W,
        client_name: &str,
        on_notification: impl FnMut(Notification),
    ) -> Result<Self, Error> {
        let params = InitializeParams {
            protocol_version: PROTOCOL_VERSION.into(),
            client_name: client_name.into(),
        };
        let mut frame = Vec::new();
        wire::send(&mut writer, 1, "initialize", &params)?;
        let info: ServerInfo = wire::reply(&mut reader, &mut frame, 1, on_notification)??;
        if info.protocol_version != PROTOCOL_VERSION {
            return Err(Error::InvalidFrame(
                "server selected another protocol version",
            ));
        }
        Ok(Self {
            reader,
            writer,
            info,
            last_id: 1,
            usable: true,
            frame,
        })
    }

    pub fn info(&self) -> &ServerInfo {
        &self.info
    }

    /// A valid RPC error consumes its reply and permits another request. A
    /// transport or framing error retires the stream because delivery can be uncertain.
    pub fn call<P: Serialize + ?Sized, T: DeserializeOwned>(
        &mut self,
        method: &str,
        params: &P,
        on_notification: impl FnMut(Notification),
    ) -> Result<T, Error> {
        if !self.usable {
            return Err(Error::Closed);
        }
        let id = self.last_id.checked_add(1).ok_or(Error::Closed)?;
        let encoded = wire::encode(id, method, params)?;
        self.usable = false;
        self.writer.write_all(&encoded)?;
        self.writer.flush()?;
        self.last_id = id;
        let reply = wire::reply(&mut self.reader, &mut self.frame, id, on_notification)?;
        self.usable = true;
        reply
    }

    /// Wait for one notification when no application request is pending.
    pub fn read_notification(&mut self) -> Result<Notification, Error> {
        if !self.usable {
            return Err(Error::Closed);
        }
        self.usable = false;
        let frame = wire::read(&mut self.reader, &mut self.frame)?;
        let note = wire::notification(frame)?;
        self.usable = true;
        Ok(note)
    }

    pub fn into_parts(self) -> (R, W) {
        (self.reader, self.writer)
    }
}

#[cfg(test)]
mod tests;
