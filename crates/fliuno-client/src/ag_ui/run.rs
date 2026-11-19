//! Bind the AG-UI text input profile to a durable local task request.
use super::{
    AgUiProjector, AgUiSseError, AgUiSseFeed, AgUiSseFeedError, AgUiSseReplay, AgUiSseStream,
    interrupt::ResumeEntry,
};
use crate::{Client, Error, Notification};
use fliuno_protocol::{
    ApprovalDetails, ApprovalRun, ApprovalRunPendingParams, ApprovalRunWindow,
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
    #[error("the request does not match the selected AG-UI input profile")]
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
pub struct AgUiRunInput {
    thread_id: String,
    pub(super) run_id: String,
    protocol_version: Option<String>,
    pub(super) parent_run_id: Option<String>,
    state: Option<Value>,
    pub(super) messages: Vec<Message>,
    #[serde(default)]
    tools: Vec<serde::de::IgnoredAny>,
    #[serde(default)]
    context: Vec<serde::de::IgnoredAny>,
    forwarded_props: Option<Value>,
    #[serde(default)]
    pub(super) resume: Vec<ResumeEntry>,
}

#[derive(Deserialize)]
pub(super) struct Message {
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

impl AgUiRunInput {
    pub fn parse(body: &[u8]) -> Result<Self, AgUiRunError> {
        if body.len() > MAX_APP_REQUEST_BYTES {
            return Err(AgUiRunError::InputTooLarge);
        }
        Ok(serde_json::from_slice(body)?)
    }

    pub fn is_resume(&self) -> bool {
        self.parent_run_id.is_some() || !self.resume.is_empty()
    }

    pub(super) fn validate(&self, session_id: &str) -> Result<(), AgUiRunError> {
        if self.thread_id != session_id || session_id.is_empty() {
            return Err(AgUiRunError::WrongThread);
        }
        validate_run_id(&self.run_id)?;
        let no_state = |field: Option<&Value>| {
            field.is_none_or(|value| value.as_object().is_some_and(|map| map.is_empty()))
        };
        if self
            .protocol_version
            .as_deref()
            .is_some_and(|version| version != "1.0")
            || !no_state(self.state.as_ref())
            || !no_state(self.forwarded_props.as_ref())
            || !self.tools.is_empty()
            || !self.context.is_empty()
            || self
                .messages
                .iter()
                .any(|message| message.encrypted_value.is_some() || !message.tool_calls.is_empty())
        {
            return Err(AgUiRunError::UnsupportedInput);
        }
        Ok(())
    }

    fn into_task(
        self,
        session_id: &str,
        interrupt_profile: bool,
    ) -> Result<(TaskStartParams, String), AgUiRunError> {
        self.validate(session_id)?;
        if self.is_resume() {
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
                client_request_id: Some(request_key(&self.run_id, interrupt_profile)),
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
    segment_start: Option<u64>,
}

impl AgUiRun {
    pub fn task(&self) -> &TaskSummary {
        &self.task
    }

    pub fn run_id(&self) -> &str {
        &self.run_id
    }

    pub fn projector(&self) -> AgUiProjector {
        AgUiProjector::for_external_run(&self.task, &self.run_id, self.segment_start)
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
        let task: TaskSummary = if self.segment_start.is_some() {
            client.call(
                "approval/cancelRun",
                &lookup(&self.task.session_id, &self.run_id, true),
                on_notification,
            )?
        } else {
            client.call(
                "task/cancelSummary",
                &TaskIdParams {
                    task_id: self.task.id.clone(),
                },
                on_notification,
            )?
        };
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

    /// Reconnect to a durable frame, using the indexed segment bounds when
    /// available. Older servers and the task-wide profile replay as before.
    pub fn reconnect_sse<R: BufRead, W: Write>(
        &self,
        client: &mut Client<R, W>,
        through: u64,
        last_event_id: Option<&str>,
        on_notification: impl FnMut(Notification),
    ) -> Result<AgUiSseReplay, AgUiSseFeedError> {
        let mut stream = self.sse(last_event_id)?;
        stream.check_watermark(through)?;
        if self.segment_start.is_some()
            && last_event_id.is_some_and(|id| !id.is_empty())
            && client.info().capabilities.get("approvalRunWindow") == Some(&Value::Bool(true))
        {
            let window: ApprovalRunWindow = client.call(
                "approval/getRunWindow",
                &lookup(&self.task.session_id, &self.run_id, true),
                on_notification,
            )?;
            stream.seek(window)?;
        }
        AgUiSseReplay::with_stream(stream, through)
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

    fn checked(
        task: TaskSummary,
        session_id: &str,
        run_id: String,
        segment_start: Option<u64>,
    ) -> Result<Self, AgUiRunError> {
        if task.session_id != session_id || task.backend != Default::default() || task.id.is_empty()
        {
            return Err(AgUiRunError::InvalidResult);
        }
        Ok(Self {
            task,
            run_id,
            segment_start,
        })
    }
}

impl<R: BufRead, W: Write> Client<R, W> {
    /// Read the pending model approval that ended this run. Hosts must authorize
    /// the reviewer before calling this method.
    pub fn get_ag_ui_interrupt_approval(
        &mut self,
        session_id: &str,
        run_id: &str,
        approval_id: &str,
        on_notification: impl FnMut(Notification),
    ) -> Result<ApprovalDetails, AgUiRunError> {
        if session_id.is_empty() {
            return Err(AgUiRunError::WrongThread);
        }
        validate_run_id(run_id)?;
        let details: ApprovalDetails = self.call(
            "approval/getRunPending",
            &ApprovalRunPendingParams {
                session_id: session_id.to_owned(),
                client_request_id: request_key(run_id, true),
                approval_id: approval_id.to_owned(),
            },
            on_notification,
        )?;
        if !matches!(&details, ApprovalDetails::ToolCall { approval } if approval.id == approval_id)
        {
            return Err(AgUiRunError::InvalidResult);
        }
        Ok(details)
    }

    /// Submit a bounded AG-UI JSON body for a session selected by the host.
    pub fn start_ag_ui_run(
        &mut self,
        session_id: &str,
        body: &[u8],
        on_notification: impl FnMut(Notification),
    ) -> Result<AgUiRun, AgUiRunError> {
        self.start_ag_ui(
            session_id,
            AgUiRunInput::parse(body)?,
            None,
            on_notification,
        )
    }

    /// Start a text run whose SSE stream stops at the first committed interrupt.
    pub fn start_ag_ui_interrupt_run(
        &mut self,
        session_id: &str,
        input: AgUiRunInput,
        on_notification: impl FnMut(Notification),
    ) -> Result<AgUiRun, AgUiRunError> {
        self.start_ag_ui(session_id, input, Some(0), on_notification)
    }

    fn start_ag_ui(
        &mut self,
        session_id: &str,
        input: AgUiRunInput,
        segment_start: Option<u64>,
        on_notification: impl FnMut(Notification),
    ) -> Result<AgUiRun, AgUiRunError> {
        let (request, run_id) = input.into_task(session_id, segment_start.is_some())?;
        let task = self.call("task/startSummary", &request, on_notification)?;
        AgUiRun::checked(task, session_id, run_id, segment_start)
    }

    /// Answer one interrupt after the host has authorized the reviewer.
    pub fn resume_ag_ui_run(
        &mut self,
        session_id: &str,
        input: AgUiRunInput,
        on_notification: impl FnMut(Notification),
    ) -> Result<AgUiRun, AgUiRunError> {
        let (request, run_id) = input.into_resume(session_id)?;
        let result: ApprovalRun = self.call("approval/resumeRun", &request, on_notification)?;
        let start = result
            .start_sequence
            .filter(|seq| (1..=i64::MAX as u64).contains(seq))
            .ok_or(AgUiRunError::InvalidResult)?;
        AgUiRun::checked(result.task, session_id, run_id, Some(start))
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
            &lookup(session_id, run_id, false),
            on_notification,
        )?;
        AgUiRun::checked(task, session_id, run_id.to_owned(), None)
    }

    /// Reattach to the original run or to a child created by a reviewer.
    pub fn attach_ag_ui_interrupt_run(
        &mut self,
        session_id: &str,
        run_id: &str,
        on_notification: impl FnMut(Notification),
    ) -> Result<AgUiRun, AgUiRunError> {
        if session_id.is_empty() {
            return Err(AgUiRunError::WrongThread);
        }
        validate_run_id(run_id)?;
        let found: ApprovalRun = self.call(
            "approval/getRun",
            &lookup(session_id, run_id, true),
            on_notification,
        )?;
        let start = found.start_sequence.unwrap_or(0);
        if start > i64::MAX as u64 || found.start_sequence == Some(0) {
            return Err(AgUiRunError::InvalidResult);
        }
        AgUiRun::checked(found.task, session_id, run_id.to_owned(), Some(start))
    }
}

fn lookup(session: &str, run_id: &str, interrupt_profile: bool) -> TaskRequestLookupParams {
    TaskRequestLookupParams {
        session_id: session.to_owned(),
        client_request_id: request_key(run_id, interrupt_profile),
    }
}

pub(super) fn validate_run_id(id: &str) -> Result<(), AgUiRunError> {
    if id.trim().is_empty() || id.len() > MAX_RUN_ID_BYTES || id.chars().any(char::is_control) {
        return Err(AgUiRunError::RunId);
    }
    Ok(())
}

pub(super) fn request_key(run_id: &str, interrupt_profile: bool) -> String {
    let prefix = if interrupt_profile {
        "ag-ui-interrupt"
    } else {
        "ag-ui"
    };
    format!("{prefix}:{:x}", Sha256::digest(run_id.as_bytes()))
}
