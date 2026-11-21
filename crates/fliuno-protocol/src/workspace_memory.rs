use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

pub const MAX_WORKSPACE_MEMORY_KEY_CHARS: usize = 128;
pub const MAX_WORKSPACE_MEMORY_CONTENT_BYTES: usize = 16 * 1024;
pub const MAX_WORKSPACE_MEMORY_QUERY_CHARS: usize = 256;
pub const MAX_MEMORY_EMBEDDING_DIMENSIONS: usize = 2048;
pub const MAX_MEMORY_INDEX_BATCH: usize = 32;
pub const MAX_MEMORY_SEMANTIC_RESULTS: u32 = 16;

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkspaceMemoryVector {
    pub memory_id: String,
    pub revision: u64,
    #[schemars(length(min = 1, max = MAX_MEMORY_EMBEDDING_DIMENSIONS))]
    pub values: Vec<f32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkspaceMemoryIndexParams {
    pub workspace_id: String,
    /// Identifies the model and its embedding recipe. Changing either requires a new identity.
    #[schemars(length(min = 1, max = 128))]
    pub model_identity: String,
    #[schemars(length(min = 1, max = MAX_MEMORY_INDEX_BATCH))]
    pub vectors: Vec<WorkspaceMemoryVector>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkspaceMemoryQueryEmbedding {
    #[schemars(length(min = 1, max = 128))]
    pub model_identity: String,
    #[schemars(length(min = 1, max = MAX_MEMORY_EMBEDDING_DIMENSIONS))]
    pub values: Vec<f32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkspaceMemorySemanticSearchParams {
    pub workspace_id: String,
    #[schemars(length(min = 1, max = MAX_WORKSPACE_MEMORY_QUERY_CHARS))]
    pub query: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub embedding: Option<WorkspaceMemoryQueryEmbedding>,
    #[schemars(range(min = 1, max = MAX_MEMORY_SEMANTIC_RESULTS))]
    pub limit: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actor_task_id: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceMemorySearchMode {
    Semantic,
    Keyword,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceMemoryFallback {
    EmbeddingUnavailable,
    NoCompatibleIndex,
    CorruptIndex,
    NoPositiveMatch,
    BelowSimilarityThreshold,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkspaceMemorySemanticMatch {
    pub memory: WorkspaceMemory,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub similarity: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkspaceMemorySemanticResult {
    pub mode: WorkspaceMemorySearchMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fallback_reason: Option<WorkspaceMemoryFallback>,
    pub items: Vec<WorkspaceMemorySemanticMatch>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkspaceMemory {
    pub workspace_id: String,
    pub id: String,
    pub key: String,
    pub content: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_task_id: Option<String>,
    pub revision: u64,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deleted_at_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkspaceMemoryPutParams {
    pub workspace_id: String,
    #[schemars(length(min = 1, max = MAX_WORKSPACE_MEMORY_KEY_CHARS))]
    pub key: String,
    #[schemars(length(min = 1, max = MAX_WORKSPACE_MEMORY_CONTENT_BYTES))]
    pub content: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_task_id: Option<String>,
    /// Required to update or restore an existing record; omit for creation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_revision: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkspaceMemoryGetParams {
    pub workspace_id: String,
    pub key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actor_task_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkspaceMemoryListParams {
    pub workspace_id: String,
    #[serde(default)]
    pub after: u64,
    #[serde(default = "super::default_limit")]
    #[schemars(range(min = 1, max = 100))]
    pub limit: u32,
    #[serde(default)]
    pub include_deleted: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actor_task_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkspaceMemorySearchParams {
    pub workspace_id: String,
    #[schemars(length(min = 1, max = MAX_WORKSPACE_MEMORY_QUERY_CHARS))]
    pub query: String,
    #[serde(default)]
    pub after: u64,
    #[serde(default = "super::default_limit")]
    #[schemars(range(min = 1, max = 100))]
    pub limit: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actor_task_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkspaceMemoryDeleteParams {
    pub workspace_id: String,
    pub key: String,
    pub expected_revision: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actor_task_id: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceMemoryAction {
    Created,
    Updated,
    Restored,
    Deleted,
    Read,
    Listed,
    Searched,
    ContextSearched,
    ContextInjected,
    Indexed,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkspaceMemoryAuditEntry {
    pub sequence: u64,
    pub workspace_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    pub action: WorkspaceMemoryAction,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_task_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actor_task_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_sha256: Option<String>,
    pub occurred_at_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkspaceMemoryAuditListParams {
    pub workspace_id: String,
    #[serde(default)]
    pub after: u64,
    #[serde(default = "super::default_limit")]
    #[schemars(range(min = 1, max = 100))]
    pub limit: u32,
}
