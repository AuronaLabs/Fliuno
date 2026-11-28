use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

pub const PROJECT_GUIDE_PATH: &str = "AGENTS.md";
pub const MAX_PROJECT_GUIDE_BYTES: usize = 8192;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkspaceProjectGuide {
    pub workspace_id: String,
    /// Zero means this workspace has never enabled a guide.
    pub revision: u64,
    pub enabled_sha256: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProjectGuideSnapshot {
    pub path: String,
    pub sha256: String,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkspaceProjectGuidePreview {
    pub state: WorkspaceProjectGuide,
    pub guide: Option<ProjectGuideSnapshot>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read_error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkspaceSetProjectGuideParams {
    pub workspace_id: String,
    pub expected_revision: u64,
    /// Null disables the guide. Otherwise, require the exact previewed digest.
    #[schemars(required, schema_with = "crate::nullable_model_profile")]
    #[serde(deserialize_with = "Deserialize::deserialize")]
    pub sha256: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProjectGuideAttribution {
    pub path: String,
    pub sha256: String,
    pub bytes: u64,
}

impl From<&ProjectGuideSnapshot> for ProjectGuideAttribution {
    fn from(guide: &ProjectGuideSnapshot) -> Self {
        Self {
            path: guide.path.clone(),
            sha256: guide.sha256.clone(),
            bytes: guide.text.len() as u64,
        }
    }
}
