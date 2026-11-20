//! Bind the AG-UI input profile to a durable local task request.
use super::{
    AgUiProjector, AgUiSseError, AgUiSseFeed, AgUiSseFeedError, AgUiSseReplay, AgUiSseStream,
    interrupt::ResumeEntry,
};
use crate::{Client, Error, Notification};
use fliuno_protocol::{
    ApprovalDetails, ApprovalRun, ApprovalRunPendingParams, ApprovalRunReplay, ApprovalRunWindow,
    ArtifactGetParams, ImageMimeType, MAX_APP_REQUEST_BYTES, MAX_TASK_ARTIFACT_INPUTS,
    MAX_TASK_MODEL_STEPS, MAX_TASK_TEXT_BYTES, TaskIdParams, TaskInput, TaskRequestLookupParams,
    TaskStartParams, TaskStatus, TaskSummary, UserContentPart, validate_multipart_input,
};
use serde::{
    Deserialize, Deserializer,
    de::{SeqAccess, Visitor},
};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    fmt,
    io::{BufRead, Write},
};

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
    #[error("invalid AG-UI inline media input: {0}")]
    MediaInput(&'static str),
    #[error("the server does not support inline image input")]
    ImageInputUnavailable,
    #[error("the server does not support inline PDF input")]
    PdfInputUnavailable,
    #[error("the server does not support stored artifact input")]
    ArtifactInputUnavailable,
    #[error("the server returned a task outside the selected session or backend")]
    InvalidResult,
    #[error("AG-UI model step limit must be 1 to {MAX_TASK_MODEL_STEPS}")]
    ModelStepLimit,
    #[error("the server does not support the requested AG-UI model step limit")]
    ModelStepLimitUnavailable,
    #[error(transparent)]
    Rpc(#[from] Error),
    #[error(transparent)]
    Replay(#[from] AgUiSseFeedError),
}

/// A host-selected bound for new model tasks. This is not an AG-UI input field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AgUiModelStepLimit(u32);

impl AgUiModelStepLimit {
    pub fn new(steps: u32) -> Result<Self, AgUiRunError> {
        if !(1..=MAX_TASK_MODEL_STEPS).contains(&steps) {
            return Err(AgUiRunError::ModelStepLimit);
        }
        Ok(Self(steps))
    }
}

/// Options for a root run, set by the host before sending it to the kernel.
#[derive(Debug, Clone, Copy, Default)]
pub struct AgUiRunOptions {
    pub interrupt_on_approval: bool,
    pub model_step_limit: Option<AgUiModelStepLimit>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AgUiRunInput {
    thread_id: String,
    pub(super) run_id: String,
    protocol_version: Option<String>,
    pub(super) parent_run_id: Option<String>,
    state: Option<Value>,
    messages: RunMessages,
    #[serde(default)]
    tools: Vec<serde::de::IgnoredAny>,
    #[serde(default)]
    context: Vec<serde::de::IgnoredAny>,
    forwarded_props: Option<ForwardedProps>,
    #[serde(default)]
    pub(super) resume: Vec<ResumeEntry>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ForwardedProps {
    fliuno: Option<FliunoForwardedProps>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct FliunoForwardedProps {
    artifact_inputs: Vec<ArtifactGetParams>,
}

struct RunMessages {
    last: Option<Message>,
    unsupported: bool,
}

impl<'de> Deserialize<'de> for RunMessages {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_seq(RunMessagesVisitor)
    }
}

struct RunMessagesVisitor;

impl<'de> Visitor<'de> for RunMessagesVisitor {
    type Value = RunMessages;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("an array of AG-UI messages")
    }

    fn visit_seq<S: SeqAccess<'de>>(self, mut seq: S) -> Result<Self::Value, S::Error> {
        let mut messages = RunMessages {
            last: None,
            unsupported: false,
        };
        while let Some(message) = seq.next_element::<Message>()? {
            // Check every message, but release prior content as soon as it is superseded.
            messages.unsupported |=
                message.encrypted_value.is_some() || !message.tool_calls.is_empty();
            messages.last = Some(message);
        }
        Ok(messages)
    }
}

#[derive(Deserialize)]
struct Message {
    #[serde(rename = "id")]
    _id: String,
    role: Role,
    content: Option<UserContent>,
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
enum UserContent {
    String(String),
    Parts(Vec<ContentPart>),
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ContentPart {
    Text { text: String },
    Image { source: InlineSource<ImageMimeType> },
    Document { source: InlineSource<PdfMimeType> },
}

#[derive(Deserialize)]
#[serde(
    tag = "type",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
enum InlineSource<M> {
    Data { value: String, mime_type: M },
}

#[derive(Deserialize)]
enum PdfMimeType {
    #[serde(rename = "application/pdf")]
    Pdf,
}

impl UserContent {
    fn into_input(self, has_artifacts: bool) -> Result<TaskInput, AgUiRunError> {
        let text: String = match self {
            Self::String(text) => text,
            Self::Parts(parts)
                if parts
                    .iter()
                    .any(|part| !matches!(part, ContentPart::Text { .. })) =>
            {
                let parts: Vec<_> = parts
                    .into_iter()
                    .map(|part| match part {
                        ContentPart::Text { text } => UserContentPart::Text { text },
                        ContentPart::Image {
                            source: InlineSource::Data { value, mime_type },
                        } => UserContentPart::Image {
                            data: value,
                            mime_type,
                        },
                        ContentPart::Document {
                            source:
                                InlineSource::Data {
                                    value,
                                    mime_type: PdfMimeType::Pdf,
                                },
                        } => UserContentPart::Pdf { data: value },
                    })
                    .collect();
                validate_multipart_input(&parts).map_err(AgUiRunError::MediaInput)?;
                return Ok(TaskInput::Parts(parts));
            }
            Self::Parts(parts) => parts
                .into_iter()
                .filter_map(|part| match part {
                    ContentPart::Text { text } => Some(text),
                    ContentPart::Image { .. } | ContentPart::Document { .. } => None,
                })
                .collect(),
        };
        if text.len() > MAX_TASK_TEXT_BYTES
            || (text.trim().is_empty() && !(has_artifacts && text.is_empty()))
        {
            return Err(AgUiRunError::UserText);
        }
        Ok(TaskInput::Text(text))
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

    /// Whether the request uses the Fliuno extension for stored attachments.
    pub fn requests_artifact_inputs(&self) -> bool {
        self.forwarded_props
            .as_ref()
            .is_some_and(|props| props.fliuno.is_some())
    }

    pub(super) fn has_messages(&self) -> bool {
        self.messages.last.is_some()
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
            || !self.tools.is_empty()
            || !self.context.is_empty()
            || self.messages.unsupported
        {
            return Err(AgUiRunError::UnsupportedInput);
        }
        Ok(())
    }

    fn into_task(
        self,
        session_id: &str,
        options: AgUiRunOptions,
    ) -> Result<(TaskStartParams, String), AgUiRunError> {
        self.validate(session_id)?;
        if self.is_resume() {
            return Err(AgUiRunError::UnsupportedInput);
        }
        let artifact_inputs = match self.forwarded_props.and_then(|props| props.fliuno) {
            Some(props)
                if (1..=MAX_TASK_ARTIFACT_INPUTS).contains(&props.artifact_inputs.len()) =>
            {
                props.artifact_inputs
            }
            Some(_) => return Err(AgUiRunError::UnsupportedInput),
            None => Vec::new(),
        };
        // The kernel already captured prior committed turns. Use only the new
        // user message; replayed client history cannot replace stored context.
        let Some(message) = self
            .messages
            .last
            .filter(|message| message.role == Role::User)
        else {
            return Err(AgUiRunError::UserText);
        };
        let input = message
            .content
            .ok_or(AgUiRunError::UserText)?
            .into_input(!artifact_inputs.is_empty())?;
        Ok((
            TaskStartParams {
                session_id: session_id.to_owned(),
                input,
                artifact_inputs,
                client_request_id: Some(request_key(&self.run_id, options.interrupt_on_approval)),
                backend: Default::default(),
                context: None,
                max_model_steps: options.model_step_limit.map(|limit| limit.0),
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

    fn checked_interrupt(
        found: ApprovalRun,
        session_id: &str,
        run_id: &str,
    ) -> Result<Self, AgUiRunError> {
        let start = found.start_sequence.unwrap_or(0);
        if start > i64::MAX as u64 || found.start_sequence == Some(0) {
            return Err(AgUiRunError::InvalidResult);
        }
        Self::checked(found.task, session_id, run_id.to_owned(), Some(start))
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
        self.start_ag_ui_input_run(
            session_id,
            AgUiRunInput::parse(body)?,
            AgUiRunOptions::default(),
            on_notification,
        )
    }

    /// Start a run whose SSE stream stops at the first committed interrupt.
    pub fn start_ag_ui_interrupt_run(
        &mut self,
        session_id: &str,
        input: AgUiRunInput,
        on_notification: impl FnMut(Notification),
    ) -> Result<AgUiRun, AgUiRunError> {
        self.start_ag_ui_input_run(
            session_id,
            input,
            AgUiRunOptions {
                interrupt_on_approval: true,
                ..AgUiRunOptions::default()
            },
            on_notification,
        )
    }

    /// Submit a parsed text run with host policy.
    pub fn start_ag_ui_text_run(
        &mut self,
        session_id: &str,
        input: AgUiRunInput,
        options: AgUiRunOptions,
        on_notification: impl FnMut(Notification),
    ) -> Result<AgUiRun, AgUiRunError> {
        let (request, run_id) = input.into_task(session_id, options)?;
        if !request.artifact_inputs.is_empty() || matches!(request.input, TaskInput::Parts(_)) {
            return Err(AgUiRunError::UnsupportedInput);
        }
        self.submit_ag_ui_run(session_id, request, run_id, options, on_notification)
    }

    /// Submit a parsed run and check required capabilities before dispatch.
    pub fn start_ag_ui_input_run(
        &mut self,
        session_id: &str,
        input: AgUiRunInput,
        options: AgUiRunOptions,
        on_notification: impl FnMut(Notification),
    ) -> Result<AgUiRun, AgUiRunError> {
        let (request, run_id) = input.into_task(session_id, options)?;
        self.submit_ag_ui_run(session_id, request, run_id, options, on_notification)
    }

    fn submit_ag_ui_run(
        &mut self,
        session_id: &str,
        request: TaskStartParams,
        run_id: String,
        options: AgUiRunOptions,
        on_notification: impl FnMut(Notification),
    ) -> Result<AgUiRun, AgUiRunError> {
        let capabilities = &self.info().capabilities;
        if !request.artifact_inputs.is_empty()
            && capabilities.get("taskArtifactInput") != Some(&Value::Bool(true))
        {
            return Err(AgUiRunError::ArtifactInputUnavailable);
        }
        if let TaskInput::Parts(parts) = &request.input {
            if capabilities.get("taskImageInput") != Some(&Value::Bool(true))
                && parts
                    .iter()
                    .any(|part| matches!(part, UserContentPart::Image { .. }))
            {
                return Err(AgUiRunError::ImageInputUnavailable);
            }
            if capabilities.get("taskPdfInput") != Some(&Value::Bool(true))
                && parts
                    .iter()
                    .any(|part| matches!(part, UserContentPart::Pdf { .. }))
            {
                return Err(AgUiRunError::PdfInputUnavailable);
            }
        }
        if let Some(steps) = request.max_model_steps
            && !self
                .info()
                .capabilities
                .get("maxTaskModelSteps")
                .and_then(Value::as_u64)
                .is_some_and(|max| max >= u64::from(steps))
        {
            return Err(AgUiRunError::ModelStepLimitUnavailable);
        }
        let task = self.call("task/startSummary", &request, on_notification)?;
        AgUiRun::checked(
            task,
            session_id,
            run_id,
            options.interrupt_on_approval.then_some(0),
        )
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
        AgUiRun::checked_interrupt(found, session_id, run_id)
    }

    /// Resolve an interrupt run and initialize its independent replay with a
    /// single RPC on servers that advertise `approvalRunReplay`.
    pub fn attach_ag_ui_interrupt_replay(
        &mut self,
        session_id: &str,
        run_id: &str,
        last_event_id: Option<&str>,
        mut on_notification: impl FnMut(Notification),
    ) -> Result<(AgUiRun, AgUiSseReplay), AgUiRunError> {
        if self.info().capabilities.get("approvalRunReplay") != Some(&Value::Bool(true)) {
            let run = self.attach_ag_ui_interrupt_run(session_id, run_id, &mut on_notification)?;
            let through = self.event_watermark(&mut on_notification)?;
            let replay = run.reconnect_sse(self, through, last_event_id, on_notification)?;
            return Ok((run, replay));
        }
        if session_id.is_empty() {
            return Err(AgUiRunError::WrongThread);
        }
        validate_run_id(run_id)?;
        let ApprovalRunReplay {
            run,
            window,
            through_sequence,
        } = self.call(
            "approval/getRunReplay",
            &lookup(session_id, run_id, true),
            on_notification,
        )?;
        let run = AgUiRun::checked_interrupt(run, session_id, run_id)?;
        let mut stream = run.sse(last_event_id).map_err(AgUiSseFeedError::from)?;
        stream.set_window(window, through_sequence)?;
        let replay = AgUiSseReplay::with_stream(stream, through_sequence)?;
        Ok((run, replay))
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
