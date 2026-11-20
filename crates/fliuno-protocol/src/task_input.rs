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
pub const MAX_INPUT_MEDIA: usize = 4;
pub const MAX_TASK_ARTIFACT_INPUTS: usize = 4;

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
    /// Convert bounded, locally stored binary data to a provider input part.
    pub fn from_media_bytes(media_type: &str, bytes: &[u8]) -> Result<Self, &'static str> {
        let image = match media_type {
            "image/png" => Some(ImageMimeType::Png),
            "image/jpeg" => Some(ImageMimeType::Jpeg),
            "image/webp" => Some(ImageMimeType::Webp),
            PDF_MIME_TYPE => None,
            _ => return Err("artifact input must be PNG, JPEG, WebP or PDF"),
        };
        if !matches_media_header(image, bytes) {
            return Err("artifact input header does not match its MIME type");
        }
        let data = STANDARD.encode(bytes);
        Ok(match image {
            Some(mime_type) => Self::Image { mime_type, data },
            None => Self::Pdf { data },
        })
    }

    /// Text for summaries, indexing and future task context. Media bytes stay in this task.
    pub fn text_projection(&self) -> &str {
        match self {
            Self::Text { text } => text,
            Self::Image { .. } => "[image]",
            Self::Pdf { .. } => "[document]",
        }
    }
}

const MEDIA_LIMIT_ERROR: &str =
    "multipart input needs 1..4 images or PDF documents with at most 512 KiB of total base64 data";

/// Shared byte and media limits for inline parts and resolved artifact inputs.
pub struct TaskInputBudget {
    text_bytes_left: usize,
    base64_bytes_left: usize,
    media_left: usize,
}

impl TaskInputBudget {
    /// Reserve slots before resolving references. Their contents are charged
    /// only after the storage layer has checked the owner and body length.
    pub fn with_parts(
        parts: &[UserContentPart],
        additional_parts: usize,
    ) -> Result<Self, &'static str> {
        if parts.len().saturating_add(additional_parts) > MAX_INPUT_PARTS {
            return Err("input must have at most 32 parts");
        }
        let mut budget = Self {
            text_bytes_left: MAX_TASK_TEXT_BYTES,
            base64_bytes_left: MAX_MEDIA_BASE64_BYTES,
            media_left: MAX_INPUT_MEDIA,
        };
        for part in parts {
            match part {
                UserContentPart::Text { text } => budget.reserve_text(text.len())?,
                UserContentPart::Image { data, .. } | UserContentPart::Pdf { data } => {
                    budget.reserve_media(data.len())?;
                }
            }
        }
        Ok(budget)
    }

    pub fn reserve_text(&mut self, bytes: usize) -> Result<(), &'static str> {
        self.text_bytes_left = self
            .text_bytes_left
            .checked_sub(bytes)
            .ok_or("input text exceeds 256 KiB")?;
        Ok(())
    }

    pub fn reserve_media(&mut self, base64_bytes: usize) -> Result<(), &'static str> {
        if self.media_left == 0 || base64_bytes > self.base64_bytes_left {
            return Err(MEDIA_LIMIT_ERROR);
        }
        self.media_left -= 1;
        self.base64_bytes_left -= base64_bytes;
        Ok(())
    }
}

/// Bound the work needed to verify or compare multipart input.
pub fn validate_multipart_input_limits(parts: &[UserContentPart]) -> Result<(), &'static str> {
    multipart_input_budget(parts, 0).map(|_| ())
}

/// Return the remaining Base64 budget when all references are known media.
pub fn multipart_input_budget(
    parts: &[UserContentPart],
    references: usize,
) -> Result<usize, &'static str> {
    let mut budget = TaskInputBudget::with_parts(parts, references)?;
    for _ in 0..references {
        budget.reserve_media(0)?;
    }
    if budget.media_left == MAX_INPUT_MEDIA {
        return Err(MEDIA_LIMIT_ERROR);
    }
    Ok(budget.base64_bytes_left)
}

/// This validates transport and MIME consistency without parsing images or documents.
pub fn validate_multipart_input(parts: &[UserContentPart]) -> Result<(), &'static str> {
    validate_multipart_input_limits(parts)?;
    validate_inline_media(parts)
}

/// Check the bytes after the caller has checked the multipart size budget.
pub fn validate_inline_media(parts: &[UserContentPart]) -> Result<(), &'static str> {
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
        if !matches_media_header(image_type.copied(), &decoded) {
            return Err("inline media header does not match its MIME type");
        }
    }
    Ok(())
}

fn matches_media_header(image: Option<ImageMimeType>, bytes: &[u8]) -> bool {
    match image {
        Some(mime_type) => mime_type.matches_header(bytes),
        None => bytes.starts_with(b"%PDF-"),
    }
}
