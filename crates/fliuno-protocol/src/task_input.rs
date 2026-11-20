//! Bounded inline media accepted at ordinary model task submission.
use base64::{Engine, engine::general_purpose::STANDARD};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

pub const MAX_TASK_TEXT_BYTES: usize = 256 * 1024;
const MAX_IMAGE_BASE64_BYTES: usize = 512 * 1024;
const MAX_INPUT_PARTS: usize = 32;
const MAX_INPUT_IMAGES: usize = 4;

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum TaskInput {
    Text(String),
    Parts(Vec<UserContentPart>),
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
}

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
    /// Text for summaries, indexing and future task context. Image bytes stay in this task.
    pub fn text_projection(&self) -> &str {
        match self {
            Self::Text { text } => text,
            Self::Image { .. } => "[image]",
        }
    }
}

/// Bound the work needed to verify or compare an image input.
pub fn validate_image_input_limits(parts: &[UserContentPart]) -> Result<(), &'static str> {
    if parts.len() > MAX_INPUT_PARTS {
        return Err("input must have at most 32 parts");
    }
    let mut text_bytes = 0;
    let mut image_bytes = 0;
    let mut images = 0;
    for part in parts {
        match part {
            UserContentPart::Text { text } => text_bytes += text.len(),
            UserContentPart::Image { data, .. } => {
                image_bytes += data.len();
                images += 1;
            }
        }
    }
    if !(1..=MAX_INPUT_IMAGES).contains(&images) || image_bytes > MAX_IMAGE_BASE64_BYTES {
        return Err("multipart input needs 1..4 images with at most 512 KiB of total base64 data");
    }
    if text_bytes > MAX_TASK_TEXT_BYTES {
        return Err("input text exceeds 256 KiB");
    }
    Ok(())
}

/// This validates transport and MIME consistency without parsing image pixels.
pub fn validate_image_input(parts: &[UserContentPart]) -> Result<(), &'static str> {
    validate_image_input_limits(parts)?;
    // Reuse one bounded scratch buffer. The encoded strings remain canonical for retries.
    let mut decoded = Vec::new();
    for part in parts {
        if let UserContentPart::Image { mime_type, data } = part {
            decoded.clear();
            STANDARD
                .decode_vec(data, &mut decoded)
                .map_err(|_| "image data must be standard padded base64")?;
            if !mime_type.matches_header(&decoded) {
                return Err("image header does not match its MIME type");
            }
        }
    }
    Ok(())
}
