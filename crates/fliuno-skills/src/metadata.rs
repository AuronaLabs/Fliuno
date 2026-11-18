//! Agent Skills frontmatter validation; metadata never changes host policy.
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SkillMetadata {
    pub name: String,
    pub description: String,
    pub license: Option<String>,
    pub compatibility: Option<String>,
    #[serde(default)]
    pub metadata: BTreeMap<String, String>,
    #[serde(rename = "allowed-tools")]
    pub allowed_tools: Option<String>,
}

pub(crate) fn parse(content: &str, folder: &str) -> Result<SkillMetadata, String> {
    let normalized = content.trim_start_matches('\u{feff}').replace("\r\n", "\n");
    let rest = normalized
        .strip_prefix("---\n")
        .ok_or("SKILL.md must begin with YAML frontmatter")?;
    let end = rest
        .find("\n---\n")
        .or_else(|| rest.strip_suffix("\n---").map(str::len))
        .ok_or("missing closing frontmatter delimiter")?;
    let meta: SkillMetadata =
        yaml_serde::from_str(&rest[..end]).map_err(|_| "invalid SKILL.md metadata")?;
    if meta.name != folder
        || meta.name.is_empty()
        || meta.name.chars().count() > 64
        || meta.name.starts_with('-')
        || meta.name.ends_with('-')
        || meta.name.contains("--")
        || !meta
            .name
            .bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
    {
        return Err("skill name must match its folder and use 1..64 lowercase letters/digits with single hyphens".into());
    }
    if meta.description.trim().is_empty()
        || meta.description.chars().count() > 1024
        || meta
            .compatibility
            .as_ref()
            .is_some_and(|s| s.chars().count() > 500)
    {
        return Err("skill description/compatibility exceeds allowed bounds".into());
    }
    Ok(meta)
}
