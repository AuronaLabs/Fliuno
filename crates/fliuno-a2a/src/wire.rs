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

/// Exactly one content field, including explicit JSON null for `data`. This
/// deliberately rejects the obsolete v0.3 `kind`/`file` representation.
fn parts(value: &Value) -> Result<&[Value]> {
    let parts = value
        .get("parts")
        .and_then(Value::as_array)
        .ok_or(Error::Protocol)?;
    if parts.is_empty() || parts.len() > 256 {
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
            // Decode only to validate the representation; never write bytes or
            // open files. The bounded HTTP body also bounds this allocation.
            // ProtoJSON permits standard or URL-safe base64, with or without
            // padding. Do not reject a canonical protobuf implementation merely
            // because it chooses another valid alphabet/padding representation.
            let config = GeneralPurposeConfig::new()
                .with_decode_padding_mode(DecodePaddingMode::Indifferent);
            let encoded = raw.as_str().ok_or(Error::Protocol)?;
            GeneralPurpose::new(&alphabet::STANDARD, config)
                .decode(encoded)
                .or_else(|_| GeneralPurpose::new(&alphabet::URL_SAFE, config).decode(encoded))
                .map_err(|_| Error::Protocol)?;
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
    pub artifacts: Vec<Value>,
}
impl Task {
    pub fn parse(raw: Value) -> Result<Self> {
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
        let artifacts = match raw.get("artifacts") {
            None | Some(Value::Null) => Vec::new(),
            Some(Value::Array(v)) if v.len() <= 64 => v.clone(),
            _ => return Err(Error::Protocol),
        };
        let mut ids = BTreeSet::new();
        for artifact in &artifacts {
            let id = required_id(artifact, "artifactId")?;
            if !ids.insert(id)
                || serde_json::to_vec(artifact)
                    .map_err(|_| Error::Protocol)?
                    .len()
                    > 262_144
            {
                return Err(Error::Budget);
            }
            parts(artifact)?;
            if artifact
                .get("name")
                .is_some_and(|n| !n.is_null() && n.as_str().is_none_or(|s| s.len() > 256))
            {
                return Err(Error::Protocol);
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
    pub(crate) fn parse(raw: Value) -> Result<Self> {
        match (raw.get("task"), raw.get("message")) {
            (Some(task), None) => Task::parse(task.clone()).map(Self::Task),
            (None, Some(message)) => Message::parse(message.clone()).map(Self::Message),
            _ => Err(Error::Protocol),
        }
    }
}
