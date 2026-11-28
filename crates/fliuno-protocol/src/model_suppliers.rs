//! Trusted local configuration RPC. Views never contain credentials or their references.
use super::{
    ModelProfileConnectionTarget, ModelProfileCredentialInput, ModelProfileCredentialStatus,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum ModelSupplierPreset {
    OpenAi,
    Anthropic,
    Gemini,
    Custom,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ModelSupplierTarget {
    New { supplier: String },
    Existing { supplier: String },
}

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ModelSupplierSaveParams {
    pub target: ModelSupplierTarget,
    pub revision: String,
    pub preset: Option<ModelSupplierPreset>,
    #[schemars(length(max = 4096))]
    pub base_url: Option<String>,
    pub credential: ModelProfileCredentialInput,
}

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ModelSupplierBindingParams {
    pub target: ModelProfileConnectionTarget,
    pub revision: String,
    pub supplier: String,
    pub protocol: String,
    pub model: String,
    pub reasoning_effort: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ModelSupplierSnapshot {
    pub revision: String,
    pub suppliers: Vec<ModelSupplierEntry>,
    pub models: Vec<ModelSupplierProfile>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ModelSupplierEntry {
    pub id: String,
    pub preset: Option<ModelSupplierPreset>,
    /// Effective API root, including the default from the preset if used.
    pub base_url: String,
    pub credential: ModelProfileCredentialStatus,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ModelSupplierProfile {
    /// Null denotes the original [model] table.
    pub profile: Option<String>,
    /// Null denotes a legacy inline connection or an unconfigured base model.
    pub supplier: Option<String>,
    pub model: Option<String>,
    pub protocol: Option<String>,
    pub reasoning_effort: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ModelSupplierRevision {
    pub revision: String,
}
