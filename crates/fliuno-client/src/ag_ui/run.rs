//! Bind the AG-UI text input profile to a durable local task request.
use super::{
    AgUiProjector, AgUiSseError, AgUiSseFeed, AgUiSseFeedError, AgUiSseReplay, AgUiSseStream,
};
use crate::{Client, Error, Notification};
use fliuno_protocol::{
    MAX_APP_REQUEST_BYTES, TaskIdParams, TaskRequestLookupParams, TaskStartParams, TaskStatus,
    TaskSummary,
};
use serde::Deserialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::io::{BufRead, Write};

const MAX_TEXT_BYTES: usize = 256 * 1024;
const MAX_RUN_ID_BYTES: usize = 512;

#[derive(Debug, thiserror::Error)]
pub enum AgUiRunError {
    #[error("AG-UI input exceeds the byte limit")]
    InputTooLarge,
    #[error("invalid AG-UI input: {0}")]
    Input(#[from] serde_json::Error),
    #[error("AG-UI threadId must match the selected local session")]
    WrongThread,
    #[error("AG-UI runId must be nonempty, at most 512 bytes and contain no control characters")]
    RunId,
    #[error("this run accepts text turns and uses the local session history and tools")]
    UnsupportedInput,
    #[error("the last AG-UI message must be nonempty user text within 256 KiB")]
    UserText,
    #[error("the server returned a task outside the selected session or backend")]
    InvalidResult,
    #[error(transparent)]
    Rpc(#[from] Error),
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Input {
    thread_id: String,
    run_id: String,
    protocol_version: Option<String>,
    parent_run_id: Option<String>,
    state: Option<Value>,
    messages: Vec<Message>,
    #[serde(default)]
    tools: Vec<serde::de::IgnoredAny>,
    #[serde(default)]
    context: Vec<serde::de::IgnoredAny>,
    forwarded_props: Option<Value>,
    #[serde(default)]
    resume: Vec<serde::de::IgnoredAny>,
}

#[derive(Deserialize)]
struct Message {
    #[serde(rename = "id")]
    _id: String,
    role: Role,
    content: TextContent,
    #[serde(rename = "encryptedValue")]
    encrypted_value: Option<serde::de::IgnoredAny>,
    #[serde(rename = "toolCalls", default)]
    tool_calls: Vec<serde::de::IgnoredAny>,
}

#[derive(Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum Role {
    User,
    Assistant,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum TextContent {
    String(String),
    Parts(Vec<TextPart>),
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum TextPart {
    Text { text: String },
}

impl TextContent {
    fn into_text(self) -> String {
        match self {
            Self::String(text) => text,
            Self::Parts(parts) => parts
                .into_iter()
                .map(|TextPart::Text { text }| text)
                .collect(),
        }
    }
}

impl Input {
    fn into_task(self, session_id: &str) -> Result<(TaskStartParams, String), AgUiRunError> {
        if self.thread_id != session_id || session_id.is_empty() {
            return Err(AgUiRunError::WrongThread);
        }
        validate_run_id(&self.run_id)?;
        let no_state = |field: Option<Value>| {
            field.is_none_or(|value| value.as_object().is_some_and(|map| map.is_empty()))
        };
        if self.parent_run_id.is_some()
            || self
                .protocol_version
                .as_deref()
                .is_some_and(|version| version != "1.0")
            || !no_state(self.state)
            || !no_state(self.forwarded_props)
            || !self.tools.is_empty()
            || !self.context.is_empty()
            || !self.resume.is_empty()
            || self
                .messages
                .iter()
                .any(|message| message.encrypted_value.is_some() || !message.tool_calls.is_empty())
        {
            return Err(AgUiRunError::UnsupportedInput);
        }
        // The kernel already captured prior committed turns. Use only the new
        // user message; replayed client history cannot replace stored context.
        let Some(message) = self
            .messages
            .into_iter()
            .last()
            .filter(|message| message.role == Role::User)
        else {
            return Err(AgUiRunError::UserText);
        };
        let text = message.content.into_text();
        if text.len() > MAX_TEXT_BYTES || text.trim().is_empty() {
            return Err(AgUiRunError::UserText);
        }
        Ok((
            TaskStartParams {
                session_id: session_id.to_owned(),
                input: text,
                client_request_id: Some(request_key(&self.run_id)),
                backend: Default::default(),
                context: None,
            },
            self.run_id,
        ))
    }
}

/// A run bound to its current committed task summary. It can rebuild projections
/// and SSE streams using the original request's run identity after reattaching.
#[derive(Debug)]
pub struct AgUiRun {
    task: TaskSummary,
    run_id: String,
}

impl AgUiRun {
    pub fn task(&self) -> &TaskSummary {
        &self.task
    }

    pub fn run_id(&self) -> &str {
        &self.run_id
    }

    pub fn projector(&self) -> AgUiProjector {
        AgUiProjector::for_external_run(&self.task, &self.run_id)
    }

    pub fn sse(&self, last_event_id: Option<&str>) -> Result<AgUiSseStream, AgUiSseError> {
        AgUiSseStream::with_projector(self.projector(), last_event_id)
    }

    /// Cancel the task bound to this run. Repeated cancellations are idempotent.
    pub fn cancel<R: BufRead, W: Write>(
        &self,
        client: &mut Client<R, W>,
        on_notification: impl FnMut(Notification),
    ) -> Result<TaskSummary, AgUiRunError> {
        let task: TaskSummary = client.call(
            "task/cancelSummary",
            &TaskIdParams {
                task_id: self.task.id.clone(),
            },
            on_notification,
        )?;
        if task.id != self.task.id
            || task.session_id != self.task.session_id
            || task.backend != self.task.backend
            || task.status != TaskStatus::Cancelled
        {
            return Err(AgUiRunError::InvalidResult);
        }
        Ok(task)
    }

    /// Replay this run through a committed watermark without changing the subscription.
    pub fn replay_sse(
        &self,
        through: u64,
        last_event_id: Option<&str>,
    ) -> Result<AgUiSseReplay, AgUiSseFeedError> {
        AgUiSseReplay::with_stream(self.sse(last_event_id)?, through)
    }

    /// Drive this run's SSE output through one connection's task subscription.
    pub fn subscribe_sse<R: BufRead, W: Write>(
        &self,
        client: &mut Client<R, W>,
        last_event_id: Option<&str>,
        on_notification: impl FnMut(Notification),
    ) -> Result<AgUiSseFeed, AgUiSseFeedError> {
        let stream = self.sse(last_event_id)?;
        AgUiSseFeed::with_stream(client, &self.task.id, stream, on_notification)
    }

    fn checked(task: TaskSummary, session_id: &str, run_id: String) -> Result<Self, AgUiRunError> {
        if task.session_id != session_id || task.backend != Default::default() || task.id.is_empty()
        {
            return Err(AgUiRunError::InvalidResult);
        }
        Ok(Self { task, run_id })
    }
}

impl<R: BufRead, W: Write> Client<R, W> {
    /// Submit a bounded AG-UI JSON body for a session selected by the host.
    pub fn start_ag_ui_run(
        &mut self,
        session_id: &str,
        body: &[u8],
        on_notification: impl FnMut(Notification),
    ) -> Result<AgUiRun, AgUiRunError> {
        if body.len() > MAX_APP_REQUEST_BYTES {
            return Err(AgUiRunError::InputTooLarge);
        }
        let input: Input = serde_json::from_slice(body)?;
        let (request, run_id) = input.into_task(session_id)?;
        let task = self.call("task/startSummary", &request, on_notification)?;
        AgUiRun::checked(task, session_id, run_id)
    }

    /// Look up the existing run without resubmitting the input or driving work.
    pub fn attach_ag_ui_run(
        &mut self,
        session_id: &str,
        run_id: &str,
        on_notification: impl FnMut(Notification),
    ) -> Result<AgUiRun, AgUiRunError> {
        if session_id.is_empty() {
            return Err(AgUiRunError::WrongThread);
        }
        validate_run_id(run_id)?;
        let task = self.call(
            "task/getSummaryByRequest",
            &TaskRequestLookupParams {
                session_id: session_id.to_owned(),
                client_request_id: request_key(run_id),
            },
            on_notification,
        )?;
        AgUiRun::checked(task, session_id, run_id.to_owned())
    }
}

fn validate_run_id(id: &str) -> Result<(), AgUiRunError> {
    if id.trim().is_empty() || id.len() > MAX_RUN_ID_BYTES || id.chars().any(char::is_control) {
        return Err(AgUiRunError::RunId);
    }
    Ok(())
}

fn request_key(run_id: &str) -> String {
    format!("ag-ui:{:x}", Sha256::digest(run_id.as_bytes()))
}
