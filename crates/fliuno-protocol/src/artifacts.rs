//! Byte ranges in the persistent artifact body.
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

pub const MAX_ARTIFACT_READ_BYTES: u32 = 64 * 1024;
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
