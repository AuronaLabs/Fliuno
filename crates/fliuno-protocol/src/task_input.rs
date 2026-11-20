//! Bounded inline media accepted at ordinary model task submission.
use base64::{Engine, engine::general_purpose::STANDARD};
use schemars::JsonSchema;
use serde::{
    Deserialize, Deserializer, Serialize,
    de::{Error, IgnoredAny, SeqAccess, Visitor},
};
use std::fmt;

pub const MAX_TASK_TEXT_BYTES: usize = 256 * 1024;
const MAX_MEDIA_BASE64_BYTES: usize = 512 * 1024;
const MAX_INPUT_PARTS: usize = 32;
const MAX_INPUT_MEDIA: usize = 4;

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(untagged)]
pub enum TaskInput {
    Text(String),
    Parts(Vec<UserContentPart>),
}

impl<'de> Deserialize<'de> for TaskInput {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(TaskInputVisitor)
    }
}

struct TaskInputVisitor;

impl<'de> Visitor<'de> for TaskInputVisitor {
    type Value = TaskInput;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a text string or an array of at most 32 input parts")
    }

    fn visit_string<E: Error>(self, text: String) -> Result<Self::Value, E> {
        Ok(TaskInput::Text(text))
    }

    fn visit_str<E: Error>(self, text: &str) -> Result<Self::Value, E> {
        self.visit_string(text.to_owned())
    }

    fn visit_seq<S: SeqAccess<'de>>(self, mut seq: S) -> Result<Self::Value, S::Error> {
        // Dispatch on the JSON shape so the text path can take ownership of
        // strings and the media path can stop before converting a large array.
        let capacity = seq.size_hint().unwrap_or(0);
        if capacity > MAX_INPUT_PARTS {
            return Err(S::Error::invalid_length(capacity, &self));
        }
        let mut parts = Vec::with_capacity(capacity);
        while parts.len() < MAX_INPUT_PARTS {
            let Some(part) = seq.next_element()? else {
                return Ok(TaskInput::Parts(parts));
            };
            parts.push(part);
        }
        if seq.next_element::<IgnoredAny>()?.is_some() {
            return Err(S::Error::invalid_length(MAX_INPUT_PARTS + 1, &self));
        }
        Ok(TaskInput::Parts(parts))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(
    tag = "type",
    rename_all = "snake_case",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum UserContentPart {
    Text {
        text: String,
    },
    Image {
        mime_type: ImageMimeType,
        data: String,
    },
    Pdf {
        data: String,
    },
}

pub const PDF_MIME_TYPE: &str = "application/pdf";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum ImageMimeType {
    #[serde(rename = "image/png")]
    Png,
    #[serde(rename = "image/jpeg")]
    Jpeg,
    #[serde(rename = "image/webp")]
    Webp,
}

impl ImageMimeType {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Png => "image/png",
            Self::Jpeg => "image/jpeg",
            Self::Webp => "image/webp",
        }
    }

    fn matches_header(self, bytes: &[u8]) -> bool {
        match self {
            Self::Png => bytes.starts_with(b"\x89PNG\r\n\x1a\n"),
            Self::Jpeg => bytes.starts_with(b"\xff\xd8\xff"),
            Self::Webp => bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP"),
        }
    }
}

impl UserContentPart {
    /// Text for summaries, indexing and future task context. Media bytes stay in this task.
    pub fn text_projection(&self) -> &str {
        match self {
            Self::Text { text } => text,
            Self::Image { .. } => "[image]",
            Self::Pdf { .. } => "[document]",
        }
    }
}

/// Bound the work needed to verify or compare multipart input.
pub fn validate_multipart_input_limits(parts: &[UserContentPart]) -> Result<(), &'static str> {
    if parts.len() > MAX_INPUT_PARTS {
        return Err("input must have at most 32 parts");
    }
    let mut text_bytes = 0;
    let mut media_bytes = 0;
    let mut media = 0;
    for part in parts {
        match part {
            UserContentPart::Text { text } => text_bytes += text.len(),
            UserContentPart::Image { data, .. } | UserContentPart::Pdf { data } => {
                media_bytes += data.len();
                media += 1;
            }
        }
    }
    if !(1..=MAX_INPUT_MEDIA).contains(&media) || media_bytes > MAX_MEDIA_BASE64_BYTES {
        return Err(
            "multipart input needs 1..4 images or PDF documents with at most 512 KiB of total base64 data",
        );
    }
    if text_bytes > MAX_TASK_TEXT_BYTES {
        return Err("input text exceeds 256 KiB");
    }
    Ok(())
}

/// This validates transport and MIME consistency without parsing images or documents.
pub fn validate_multipart_input(parts: &[UserContentPart]) -> Result<(), &'static str> {
    validate_multipart_input_limits(parts)?;
    // Reuse one bounded scratch buffer. The encoded strings remain canonical for retries.
    let mut decoded = Vec::new();
    for part in parts {
        let (data, image_type) = match part {
            UserContentPart::Text { .. } => continue,
            UserContentPart::Image { data, mime_type } => (data, Some(mime_type)),
            UserContentPart::Pdf { data } => (data, None),
        };
        decoded.clear();
        STANDARD
            .decode_vec(data, &mut decoded)
            .map_err(|_| "inline media must use standard padded base64")?;
        let matches = match image_type {
            Some(mime_type) => mime_type.matches_header(&decoded),
            None => decoded.starts_with(b"%PDF-"),
        };
        if !matches {
            return Err("inline media header does not match its MIME type");
        }
    }
    Ok(())
}
