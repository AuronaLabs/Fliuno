//! Display metadata stays separate from the canonical workspace identity.
use crate::Workspace;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

pub const MAX_WORKSPACE_TITLE_CHARS: usize = 120;

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkspaceProfile {
    pub workspace: Workspace,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(min = 1, max = MAX_WORKSPACE_TITLE_CHARS))]
    pub title: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkspaceSetTitleParams {
    pub workspace_id: String,
    /// Explicitly pass an empty string to remove the title.
    #[schemars(length(max = MAX_WORKSPACE_TITLE_CHARS))]
    pub title: String,
}
