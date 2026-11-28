//! Update the startup model without re-serializing the rest of config.toml.
use super::{
    ModelSettings, Settings,
    config_edit::{ConfigEdit, replace_value},
};
use std::path::Path;
use toml_edit::{Item, Table, value};

impl Settings {
    pub fn save_default_model(
        path: &Path,
        profile: Option<&str>,
        validate: impl FnOnce(&ModelSettings) -> Result<(), String>,
    ) -> Result<(), String> {
        let edit = ConfigEdit::open(path)?;
        let settings = edit.settings()?;
        let model = profile.map_or(Ok(&settings.model), |id| settings.named_model(id))?;
        validate(model)?;
        if settings.workbench.model_profile.as_deref() == profile {
            return Ok(());
        }
        let mut doc = edit.document()?;
        if let Some(profile) = profile {
            let item = doc.entry("workbench").or_insert(Item::Table(Table::new()));
            let table = item
                .as_table_like_mut()
                .ok_or("Invalid workbench settings")?;
            replace_value(table, "model_profile", value(profile));
        } else if let Some(table) = doc.get_mut("workbench").and_then(Item::as_table_like_mut) {
            table.remove("model_profile");
        }
        let output = doc.to_string();
        let updated = edit.parse(&output)?;
        if updated.workbench.model_profile.as_deref() != profile {
            return Err("Could not update the startup model default".into());
        }
        edit.commit(&output)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    const VALID: &str = r#"# profile notes
[workbench]
theme = 'dark'
model_profile = 'one' # startup choice

[model]
provider = 'openai'
name = 'base-model'

[models.one]
provider = 'openai'
name = 'first-model'

[models.two]
provider = 'anthropic'
name = 'second-model'
api_key_file = 'key.txt'
"#;

    #[test]
    fn changes_only_the_default_and_can_return_to_the_base_model() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(&path, VALID).unwrap();
        Settings::save_default_model(&path, Some("two"), |model| {
            assert_eq!(model.name.as_deref(), Some("second-model"));
            assert_eq!(
                model.api_key_file.as_deref(),
                Some(dir.path().join("key.txt").as_path())
            );
            Ok(())
        })
        .unwrap();
        let edited = fs::read_to_string(&path).unwrap();
        assert_eq!(
            edited,
            VALID.replace("'one' # startup choice", "\"two\" # startup choice")
        );
        let settings = Settings::load(&path).unwrap();
        assert_eq!(
            settings.model_for_launch(None).unwrap().name.as_deref(),
            Some("second-model")
        );
        assert_eq!(
            settings
                .model_for_launch(Some("one"))
                .unwrap()
                .name
                .as_deref(),
            Some("first-model")
        );
        Settings::save_default_model(&path, None, |model| {
            assert_eq!(model.name.as_deref(), Some("base-model"));
            Ok(())
        })
        .unwrap();
        assert!(
            Settings::load(&path)
                .unwrap()
                .workbench
                .model_profile
                .is_none()
        );
    }

    #[test]
    fn preserves_a_concurrent_external_edit_and_rejects_an_invalid_profile() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(&path, VALID).unwrap();
        assert!(Settings::save_default_model(&path, Some("missing"), |_| Ok(())).is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), VALID);
        let external = format!("# external change\n{VALID}");
        let error = Settings::save_default_model(&path, Some("two"), |_| {
            fs::write(&path, &external).unwrap();
            Ok(())
        })
        .unwrap_err();
        assert!(error.contains("changed"), "{error}");
        assert_eq!(fs::read_to_string(&path).unwrap(), external);
    }

    #[test]
    fn handles_an_inline_workbench_and_a_configuration_without_a_workbench() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        for workbench in ["workbench = { theme = 'dark' }\n", ""] {
            let models = VALID.split_once("[model]").unwrap().1;
            fs::write(&path, format!("{workbench}[model]{models}")).unwrap();
            Settings::save_default_model(&path, Some("two"), |_| Ok(())).unwrap();
            let saved = fs::read_to_string(&path).unwrap();
            assert!(saved.contains(models));
            assert_eq!(
                Settings::load(&path)
                    .unwrap()
                    .workbench
                    .model_profile
                    .as_deref(),
                Some("two")
            );
        }
    }
}
