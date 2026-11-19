//! Only the supported domain subset is typed. Original messages/artifacts stay
//! JSON values so optional metadata and future fields survive artifact storage.
use crate::{Error, Result};
use base64::{
    Engine, alphabet,
    engine::{DecodePaddingMode, GeneralPurpose, GeneralPurposeConfig},
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeSet;

mod artifact;
pub use artifact::ArtifactPayload;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum TaskState {
    #[serde(rename = "TASK_STATE_UNSPECIFIED")]
    Unspecified,
    #[serde(rename = "TASK_STATE_SUBMITTED")]
    Submitted,
    #[serde(rename = "TASK_STATE_WORKING")]
    Working,
    #[serde(rename = "TASK_STATE_COMPLETED")]
    Completed,
    #[serde(rename = "TASK_STATE_FAILED")]
    Failed,
    #[serde(rename = "TASK_STATE_CANCELED")]
    Canceled,
    #[serde(rename = "TASK_STATE_INPUT_REQUIRED")]
    InputRequired,
    #[serde(rename = "TASK_STATE_REJECTED")]
    Rejected,
    #[serde(rename = "TASK_STATE_AUTH_REQUIRED")]
    AuthRequired,
}
impl TaskState {
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Completed | Self::Failed | Self::Canceled | Self::Rejected
        )
    }
    pub fn is_running(self) -> bool {
        matches!(self, Self::Submitted | Self::Working)
    }
    pub fn is_settled(self) -> bool {
        self.is_terminal() || matches!(self, Self::InputRequired | Self::AuthRequired)
    }
    pub fn allows_transition_to(self, next: Self) -> bool {
        if self == next {
            return self != Self::Unspecified;
        }
        if self.is_terminal() || self == Self::Unspecified {
            return false;
        }
        if matches!(self, Self::InputRequired | Self::AuthRequired) {
            return matches!(next, Self::Canceled | Self::Failed);
        }
        matches!(
            next,
            Self::Working
                | Self::Completed
                | Self::Failed
                | Self::Canceled
                | Self::InputRequired
                | Self::Rejected
                | Self::AuthRequired
        )
    }
}

pub fn valid_id(value: &str) -> bool {
    !value.is_empty() && value.len() <= 256 && !value.chars().any(char::is_control)
}

fn optional_id(value: &Value, key: &str) -> Result<Option<String>> {
    match value.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) if s.is_empty() => Ok(None),
        Some(Value::String(s)) if valid_id(s) => Ok(Some(s.clone())),
        _ => Err(Error::Protocol),
    }
}
fn required_id(value: &Value, key: &str) -> Result<String> {
    optional_id(value, key)?.ok_or(Error::Protocol)
}

/// ProtoJSON bytes accept either Base64 alphabet, with optional padding.
pub fn decode_raw_part(encoded: &str) -> Result<Vec<u8>> {
    const CONFIG: GeneralPurposeConfig =
        GeneralPurposeConfig::new().with_decode_padding_mode(DecodePaddingMode::Indifferent);
    const STANDARD: GeneralPurpose = GeneralPurpose::new(&alphabet::STANDARD, CONFIG);
    const URL_SAFE: GeneralPurpose = GeneralPurpose::new(&alphabet::URL_SAFE, CONFIG);
    STANDARD
        .decode(encoded)
        .or_else(|_| URL_SAFE.decode(encoded))
        .map_err(|_| Error::Protocol)
}

/// Exactly one content field, including explicit JSON null for `data`. This
/// deliberately rejects the obsolete v0.3 `kind`/`file` representation.
fn parts(value: &Value) -> Result<&[Value]> {
    let parts = value
        .get("parts")
        .and_then(Value::as_array)
        .ok_or(Error::Protocol)?;
    if parts.is_empty() || parts.len() > artifact::MAX_PARTS {
        return Err(Error::Protocol);
    }
    for part in parts {
        let map = part.as_object().ok_or(Error::Protocol)?;
        if ["text", "raw", "url", "data"]
            .iter()
            .filter(|key| map.contains_key(**key))
            .count()
            != 1
            || map.contains_key("kind")
            || map.contains_key("file")
        {
            return Err(Error::Protocol);
        }
        for key in ["text", "raw", "url", "mediaType", "filename"] {
            if let Some(value) = map.get(key)
                && !value.is_string()
            {
                return Err(Error::Protocol);
            }
        }
        if let Some(raw) = map.get("raw") {
            decode_raw_part(raw.as_str().ok_or(Error::Protocol)?)?;
        }
    }
    Ok(parts)
}

#[derive(Debug, Clone)]
pub struct Message {
    pub id: String,
    pub context_id: Option<String>,
    pub task_id: Option<String>,
    pub raw: Value,
}
impl Message {
    pub fn parse(raw: Value) -> Result<Self> {
        if raw.get("role").and_then(Value::as_str) != Some("ROLE_AGENT") {
            return Err(Error::Protocol);
        }
        parts(&raw)?;
        Ok(Self {
            id: required_id(&raw, "messageId")?,
            context_id: optional_id(&raw, "contextId")?,
            task_id: optional_id(&raw, "taskId")?,
            raw,
        })
    }
    pub fn text(&self) -> String {
        text_parts(&self.raw)
    }
}

pub fn text_parts(raw: &Value) -> String {
    raw.get("parts")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|part| part.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("\n")
}

#[derive(Debug, Clone)]
pub struct Task {
    pub id: String,
    pub context_id: Option<String>,
    pub state: TaskState,
    pub status_message: Option<Message>,
    pub artifacts: Vec<ArtifactPayload>,
}
impl Task {
    pub fn parse(mut raw: Value) -> Result<Self> {
        let id = required_id(&raw, "id")?;
        let context_id = optional_id(&raw, "contextId")?;
        let state = serde_json::from_value(
            raw.pointer("/status/state")
                .cloned()
                .ok_or(Error::Protocol)?,
        )
        .map_err(|_| Error::Protocol)?;
        let status_message = raw
            .pointer("/status/message")
            .filter(|v| !v.is_null())
            .cloned()
            .map(Message::parse)
            .transpose()?;
        if let Some(message) = &status_message
            && (message.task_id.as_ref().is_some_and(|t| t != &id)
                || message
                    .context_id
                    .as_ref()
                    .is_some_and(|c| Some(c) != context_id.as_ref()))
        {
            return Err(Error::Protocol);
        }
        let artifacts = match raw.get_mut("artifacts").map(Value::take) {
            None | Some(Value::Null) => Vec::new(),
            Some(Value::Array(v)) if v.len() <= 64 => v,
            _ => return Err(Error::Protocol),
        }
        .into_iter()
        .map(ArtifactPayload::parse)
        .collect::<Result<Vec<_>>>()?;
        let mut ids = BTreeSet::new();
        for artifact in &artifacts {
            if !ids.insert(artifact.id()) {
                return Err(Error::Budget);
            }
        }
        Ok(Self {
            id,
            context_id,
            state,
            status_message,
            artifacts,
        })
    }
    /// Poll responses and cancellation acknowledgements must stay bound to the
    /// first accepted identity; peer-provided IDs never select a different task.
    pub fn matches(&self, previous: &Self) -> bool {
        self.id == previous.id && self.context_id == previous.context_id
    }
}

#[derive(Debug, Clone)]
pub enum SendResponse {
    Task(Task),
    Message(Message),
}
impl SendResponse {
    pub(crate) fn parse(mut raw: Value) -> Result<Self> {
        let object = raw.as_object_mut().ok_or(Error::Protocol)?;
        match (object.remove("task"), object.remove("message")) {
            (Some(task), None) => Task::parse(task).map(Self::Task),
            (None, Some(message)) => Message::parse(message).map(Self::Message),
            _ => Err(Error::Protocol),
        }
    }
}

#[derive(Debug, Clone)]
pub enum StreamEvent {
    Task(Task),
    Message(Message),
    StatusUpdate {
        task_id: String,
        context_id: Option<String>,
        state: TaskState,
        message: Option<Message>,
    },
    ArtifactUpdate {
        task_id: String,
        context_id: Option<String>,
        artifact: ArtifactPayload,
        append: bool,
        last_chunk: Option<bool>,
    },
}

impl StreamEvent {
    pub(crate) fn parse(mut raw: Value) -> Result<Self> {
        let object = raw.as_object_mut().ok_or(Error::Protocol)?;
        let variants = ["task", "message", "statusUpdate", "artifactUpdate"];
        let present = variants
            .iter()
            .filter(|key| object.contains_key(**key))
            .count();
        if present != 1 {
            return Err(Error::Protocol);
        }
        if let Some(task) = object.remove("task") {
            return Task::parse(task).map(Self::Task);
        }
        if let Some(message) = object.remove("message") {
            return Message::parse(message).map(Self::Message);
        }
        if let Some(status) = object.get("statusUpdate") {
            let task_id = required_id(status, "taskId")?;
            let context_id = optional_id(status, "contextId")?;
            let status_value = status.get("status").cloned().ok_or(Error::Protocol)?;
            let task = Task::parse(serde_json::json!({
                "id":task_id,
                "contextId":context_id,
                "status":status_value,
            }))?;
            return Ok(Self::StatusUpdate {
                task_id: task.id,
                context_id: task.context_id,
                state: task.state,
                message: task.status_message,
            });
        }
        let mut update = object.remove("artifactUpdate").ok_or(Error::Protocol)?;
        let task_id = required_id(&update, "taskId")?;
        let context_id = optional_id(&update, "contextId")?;
        let artifact = ArtifactPayload::parse(
            update
                .get_mut("artifact")
                .map(Value::take)
                .ok_or(Error::Protocol)?,
        )?;
        Ok(Self::ArtifactUpdate {
            task_id,
            context_id,
            artifact,
            append: optional_bool(&update, "append")?.unwrap_or(false),
            last_chunk: optional_bool(&update, "lastChunk")?,
        })
    }
}

fn optional_bool(value: &Value, key: &str) -> Result<Option<bool>> {
    match value.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Bool(value)) => Ok(Some(*value)),
        _ => Err(Error::Protocol),
    }
}
