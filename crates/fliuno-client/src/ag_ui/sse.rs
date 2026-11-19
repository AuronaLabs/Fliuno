//! SSE framing for a task's durable AG-UI projection.
use super::{AgUiProjectionError, AgUiProjector, AgUiRunPhase};
use fliuno_protocol::{Event, TaskSummary};
use std::{
    cmp::Ordering,
    io::{self, Write},
};

pub const AG_UI_SSE_CONTENT_TYPE: &str = "text/event-stream";

#[derive(Debug, thiserror::Error)]
pub enum AgUiSseError {
    #[error("task ID cannot be encoded as an SSE event ID")]
    InvalidTaskId,
    #[error("invalid Last-Event-ID for this task")]
    InvalidResumeId,
    #[error("Last-Event-ID was not found in this task's committed projection")]
    UnknownResumeId,
    #[error("the SSE stream stopped after a prior error")]
    Closed,
    #[error(transparent)]
    Projection(#[from] AgUiProjectionError),
    #[error("encoding an AG-UI SSE frame: {0}")]
    Encoding(#[from] serde_json::Error),
    #[error("writing an AG-UI SSE frame: {0}")]
    Output(#[from] io::Error),
}

#[derive(Clone, Copy)]
struct FramePosition {
    sequence: u64,
    ordinal: usize,
}

impl FramePosition {
    fn parse(task: &str, id: &str) -> Option<Self> {
        let (task_prefix, rest) = id.split_once(':')?;
        if task_prefix != task {
            return None;
        }
        let (sequence, ordinal) = rest.split_once(':')?;
        let sequence_number = sequence.parse::<u64>().ok()?;
        let ordinal_number = ordinal.parse::<usize>().ok()?;
        if sequence_number == 0
            || ordinal_number == 0
            || sequence != sequence_number.to_string()
            || ordinal != ordinal_number.to_string()
        {
            return None;
        }
        Some(Self {
            sequence: sequence_number,
            ordinal: ordinal_number,
        })
    }
}

/// Writes one task's projected events as SSE. Feed committed events in order,
/// from `TaskStarted` even when reconnecting. Use a fresh stream and the last
/// fully received frame ID after any partial transport failure.
pub struct AgUiSseStream {
    projector: AgUiProjector,
    resume: Option<FramePosition>,
    closed: bool,
    frame: Vec<u8>,
}

impl AgUiSseStream {
    /// Pass the consumer's `Last-Event-ID` when reconnecting to this task.
    pub fn new(task: &TaskSummary, last_event_id: Option<&str>) -> Result<Self, AgUiSseError> {
        Self::with_projector(AgUiProjector::new(task), last_event_id)
    }

    pub(super) fn with_projector(
        projector: AgUiProjector,
        last_event_id: Option<&str>,
    ) -> Result<Self, AgUiSseError> {
        if projector.task_id.is_empty()
            || !projector
                .task_id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        {
            return Err(AgUiSseError::InvalidTaskId);
        }
        let resume = match last_event_id {
            Some(id) if !id.is_empty() => Some(
                FramePosition::parse(&projector.task_id, id)
                    .ok_or(AgUiSseError::InvalidResumeId)?,
            ),
            _ => None,
        };
        Ok(Self {
            projector,
            resume,
            closed: false,
            frame: Vec::new(),
        })
    }

    pub(super) fn task_id(&self) -> &str {
        &self.projector.task_id
    }

    pub(super) fn run_phase(&self) -> AgUiRunPhase {
        self.projector.run_phase()
    }

    pub(super) fn check_watermark(&self, through: u64) -> Result<(), AgUiSseError> {
        if self
            .resume
            .is_some_and(|position| position.sequence > through)
        {
            return Err(AgUiSseError::UnknownResumeId);
        }
        Ok(())
    }

    /// Verify that replay located the resume frame before handing off to live
    /// events. Use the watermark from the task's event subscription.
    pub fn finish_replay(&mut self) -> Result<(), AgUiSseError> {
        if self.closed {
            return Err(AgUiSseError::Closed);
        }
        if self.resume.is_some() {
            self.closed = true;
            return Err(AgUiSseError::UnknownResumeId);
        }
        Ok(())
    }

    /// Project one source event and write all its SSE frames, returning the frame
    /// count. Flush the destination at the cadence appropriate to its transport.
    pub fn write_event(
        &mut self,
        event: Event,
        output: &mut impl Write,
    ) -> Result<usize, AgUiSseError> {
        if self.closed {
            return Err(AgUiSseError::Closed);
        }
        let result = self.write_projected(event, output);
        self.closed = result.is_err();
        result
    }

    fn write_projected(
        &mut self,
        event: Event,
        output: &mut impl Write,
    ) -> Result<usize, AgUiSseError> {
        let sequence = event.sequence;
        let projected = self.projector.project(event)?;
        let skip = match self.resume {
            Some(position) => match sequence.cmp(&position.sequence) {
                Ordering::Less => return Ok(0),
                Ordering::Equal if position.ordinal <= projected.len() => {
                    self.resume = None;
                    position.ordinal
                }
                _ => return Err(AgUiSseError::UnknownResumeId),
            },
            None => 0,
        };
        let count = projected.len() - skip;
        for (index, event) in projected.into_iter().enumerate().skip(skip) {
            self.frame.clear();
            write!(
                self.frame,
                "id: {}:{sequence}:{}\ndata: ",
                self.projector.task_id,
                index + 1
            )?;
            serde_json::to_writer(&mut self.frame, &event)?;
            self.frame.extend_from_slice(b"\n\n");
            output.write_all(&self.frame)?;
        }
        Ok(count)
    }
}
