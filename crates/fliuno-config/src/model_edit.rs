//! Edit the model ID and effort of a saved profile without touching credentials.
use super::{
    ModelSettings, Provider, ReasoningEffort, Settings,
    config_edit::{CONFIG_LIMIT, ConfigEdit, replace_value, revision},
    read_file, validate_profile_id,
};
use std::path::Path;
use toml_edit::{Item, value};

pub struct ModelEdit<'a> {
    pub profile: Option<&'a str>,
    pub revision: &'a str,
    pub model: &'a str,
    pub reasoning_effort: Option<ReasoningEffort>,
}

impl Settings {
    /// The revision binds edits to the exact file the user saw. Callers should
    /// expose only non-secret display fields, not the credential location.
    pub fn read_model_for_edit(
        path: &Path,
        profile: Option<&str>,
    ) -> Result<(String, ModelSettings), String> {
        let text = read_file(path, CONFIG_LIMIT).map_err(|_| "Cannot read model configuration")?;
        let settings = Self::parse(path, &text)?;
        Ok((revision(&text), editable_model(settings, profile)?))
    }

    pub fn save_model_edit(
        path: &Path,
        change: ModelEdit<'_>,
        validate: impl FnOnce(&ModelSettings) -> Result<(), String>,
    ) -> Result<(String, ModelSettings), String> {
        let edit = ConfigEdit::open(path)?;
        edit.require_revision(change.revision)?;
        let name = validate_model_name(change.model)?;
        let current = editable_model(edit.settings()?, change.profile)?;
        if current.name.as_deref() == Some(name)
            && current.reasoning_effort == change.reasoning_effort
        {
            validate(&current)?;
            return Ok((change.revision.to_owned(), current));
        }
        let mut doc = edit.document()?;
        let table = match change.profile {
            Some(id) => doc
                .get_mut("models")
                .and_then(Item::as_table_like_mut)
                .and_then(|models| models.get_mut(id)),
            None => doc.get_mut("model"),
        }
        .and_then(Item::as_table_like_mut)
        .ok_or("Model profile is missing")?;
        if current.name.as_deref() != Some(name) {
            replace_value(table, "name", value(name));
        }
        if current.reasoning_effort != change.reasoning_effort {
            if let Some(effort) = change.reasoning_effort {
                replace_value(table, "reasoning_effort", value(effort.as_str()));
            } else {
                table.remove("reasoning_effort");
            }
        }
        let output = doc.to_string();
        let model = editable_model(edit.parse(&output)?, change.profile)?;
        model.validate_reasoning_effort()?;
        validate(&model)?;
        edit.commit(&output)?;
        Ok((revision(&output), model))
    }
}

fn editable_model(mut settings: Settings, profile: Option<&str>) -> Result<ModelSettings, String> {
    let model = if let Some(id) = profile {
        validate_profile_id(id)?;
        settings
            .models
            .remove(id)
            .ok_or_else(|| format!("Unknown model profile {id}"))?
    } else {
        settings.model
    };
    if matches!(model.provider, Provider::Demo) {
        return Err("Choose a configured live model".into());
    }
    Ok(model)
}

pub(super) fn validate_model_name(model: &str) -> Result<&str, String> {
    let model = model.trim();
    if model.is_empty() || model.len() > 256 || model.chars().any(char::is_control) {
        return Err("Enter a model ID of at most 256 bytes without control characters.".into());
    }
    Ok(model)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn updates_only_the_target_fields_and_never_overwrites_a_stale_revision() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("config.toml");
        let original = r#"# user's comment
[workbench]
model_profile = 'lab'
[models.lab]
provider = 'anthropic'
name = 'old' # keep this note
reasoning_effort = 'high'
api_key_file = 'fixture.key'
[models.lab.budget]
window = 2048
max_output_tokens = 256
[models.other]
provider = 'openai'
name = 'separate'
"#;
        fs::write(&path, original).unwrap();
        let (revision, before) = Settings::read_model_for_edit(&path, Some("lab")).unwrap();
        assert_eq!(before.name.as_deref(), Some("old"));
        let patch = |effort| ModelEdit {
            profile: Some("lab"),
            revision: &revision,
            model: "new",
            reasoning_effort: effort,
        };
        let invalid =
            Settings::save_model_edit(&path, patch(Some(ReasoningEffort::Minimal)), |_| Ok(()));
        assert!(invalid.is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), original);
        let (after_revision, after) =
            Settings::save_model_edit(&path, patch(None), |_| Ok(())).unwrap();
        assert_ne!(revision, after_revision);
        assert_eq!(after.reasoning_effort, None);
        assert_eq!(after.api_key_file, before.api_key_file);
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            original
                .replace("'old' # keep this note", "\"new\" # keep this note")
                .replace("reasoning_effort = 'high'\n", ""),
        );
        assert!(
            Settings::save_model_edit(&path, patch(None), |_| Ok(()))
                .unwrap_err()
                .contains("changed")
        );
    }
}
