use super::{
    Language, Settings,
    config_edit::{ConfigEdit, replace_value},
};
use std::path::Path;
use toml_edit::{Item, Table, value};

impl Settings {
    /// Save only the local presentation preference, keeping other settings and comments.
    pub fn save_workbench_language(path: &Path, language: Language) -> Result<(), String> {
        let edit = ConfigEdit::open(path)?;
        if edit.settings()?.workbench.language == language {
            return Ok(());
        }
        let mut doc = edit.document()?;
        let table = doc
            .entry("workbench")
            .or_insert(Item::Table(Table::new()))
            .as_table_like_mut()
            .ok_or("Invalid workbench settings")?;
        replace_value(table, "language", value(language.id()));
        let output = doc.to_string();
        edit.parse(&output)?;
        edit.commit(&output)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn changes_only_language_and_preserves_inline_settings() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("config.toml");
        let original = "workbench = { theme = 'dark', max_model_steps = 16 } # keep\n\n[model]\nprovider = 'openai'\nname = 'selected'\napi_key_file = 'key.txt'\n";
        fs::write(&path, original).unwrap();
        assert_eq!(
            Settings::load(&path).unwrap().workbench.language,
            Language::English
        );
        Settings::save_workbench_language(&path, Language::SimplifiedChinese).unwrap();
        let written = fs::read_to_string(&path).unwrap();
        let settings = Settings::load(&path).unwrap();
        assert_eq!(settings.workbench.language, Language::SimplifiedChinese);
        assert_eq!(settings.workbench.max_model_steps, Some(16));
        assert_eq!(settings.model.name.as_deref(), Some("selected"));
        assert!(written.contains("# keep"));
        assert_eq!(
            written.split_once("[model]").unwrap().1,
            original.split_once("[model]").unwrap().1
        );
        Settings::save_workbench_language(&path, Language::English).unwrap();
        assert_eq!(
            Settings::load(&path).unwrap().workbench.language,
            Language::English
        );
        assert!(!root.path().join("key.txt").exists());
    }
}
