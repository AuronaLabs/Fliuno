//! Byte ranges in the persistent artifact body.
use crate::ArtifactSummary;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(
    tag = "kind",
    rename_all = "snake_case",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum ArtifactCatalogScope {
    Session { session_id: String },
    Workspace { workspace_id: String },
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ArtifactCatalogParams {
    pub scope: ArtifactCatalogScope,
    /// Match the stored media type exactly, including any parameters.
    #[serde(default)]
    pub media_type: Option<String>,
    #[serde(default)]
    pub after: u64,
    #[serde(default = "super::default_limit")]
    #[schemars(range(min = 1, max = 100))]
    pub limit: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ArtifactCatalogSnapshotParams {
    pub scope: ArtifactCatalogScope,
    /// Match the stored media type exactly, including any parameters.
    #[serde(default)]
    pub media_type: Option<String>,
    /// Exclusive cursor from the previous page, or zero for the first page.
    #[serde(default)]
    pub after: u64,
    /// Inclusive catalog watermark. Omit it on the first page, then reuse it.
    #[serde(default)]
    pub through_sequence: Option<u64>,
    #[serde(default = "super::default_limit")]
    #[schemars(range(min = 1, max = 100))]
    pub limit: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ArtifactCatalogSnapshotPage {
    pub items: Vec<ArtifactSummary>,
    pub through_sequence: u64,
    pub next_after: u64,
    pub has_more: bool,
}

pub const MAX_ARTIFACT_READ_BYTES: u32 = 64 * 1024;
pub const MAX_ARTIFACT_DIGEST_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_BINARY_ARTIFACT_BYTES: usize = 256 * 1024;

/// Text keeps its original wire field. Binary bytes are encoded only at the
/// JSON boundary; callers can also fetch raw ranges using artifacts/read.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum ArtifactBody {
    Text {
        content: String,
    },
    Binary {
        #[serde(rename = "contentBase64", with = "base64_body")]
        #[schemars(with = "String")]
        bytes: Vec<u8>,
    },
}

impl ArtifactBody {
    pub fn as_bytes(&self) -> &[u8] {
        match self {
            Self::Text { content } => content.as_bytes(),
            Self::Binary { bytes } => bytes,
        }
    }
}

mod base64_body {
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    use serde::{Deserialize, Deserializer, Serializer, de::Error};

    pub(super) fn serialize<S: Serializer>(bytes: &[u8], serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(&base64::display::Base64Display::new(bytes, &STANDARD))
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Vec<u8>, D::Error> {
        let text = std::borrow::Cow::<str>::deserialize(deserializer)?;
        STANDARD.decode(text.as_bytes()).map_err(D::Error::custom)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ArtifactDigest {
    pub content_bytes: u64,
    pub sha256: String,
}
const fn default_artifact_read_bytes() -> u32 {
    32 * 1024
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ArtifactReadParams {
    pub task_id: String,
    pub artifact_id: String,
    #[serde(default)]
    pub after: u64,
    #[serde(default = "default_artifact_read_bytes")]
    #[schemars(range(min = 1, max = MAX_ARTIFACT_READ_BYTES))]
    pub limit: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ArtifactChunk {
    pub offset: u64,
    pub next_offset: u64,
    pub content_bytes: u64,
    pub data_base64: String,
}
