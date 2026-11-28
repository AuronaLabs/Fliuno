//! Persistence for the first live profile, shared by all local clients.
use super::{
    ConnectionCredential, ModelConnectionDraft, ModelProtocol, Provider, ReasoningEffort, Settings,
};
use std::{fs, io, io::Write, path::Path};
use toml_edit::{DocumentMut, Item, Table, value};

pub struct ModelSetup<'a> {
    pub provider: Provider,
    pub protocol: ModelProtocol,
    pub model: &'a str,
    pub base_url: Option<&'a str>,
    pub reasoning_effort: Option<ReasoningEffort>,
    pub credential: ModelSetupCredential<'a>,
}

pub enum ModelSetupCredential<'a> {
    Managed(&'a str),
    File(&'a str),
    None,
}

impl Settings {
    /// Create a new startup configuration. Never replace an existing file.
    pub fn create_live_model(path: &Path, setup: ModelSetup<'_>) -> Result<(), String> {
        let parent = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        let model = super::model_edit::validate_model_name(setup.model)?;
        if setup
            .reasoning_effort
            .is_some_and(|effort| !setup.provider.reasoning_efforts().contains(&effort))
        {
            return Err("The model provider does not accept this reasoning effort".into());
        }
        let draft = ModelConnectionDraft {
            provider: setup.provider,
            protocol: setup.protocol,
            base_url: setup.base_url,
            credential: match setup.credential {
                ModelSetupCredential::Managed(key) => ConnectionCredential::Managed(key),
                ModelSetupCredential::File(file) => ConnectionCredential::File(file),
                ModelSetupCredential::None => ConnectionCredential::None,
            },
        };
        let mut profile = Table::new();
        let pending = draft.stage(path, &mut profile, None)?;
        profile.insert("name", value(model));
        if let Some(effort) = setup.reasoning_effort {
            profile.insert("reasoning_effort", value(effort.as_str()));
        }
        let mut document = DocumentMut::new();
        document.insert("model", Item::Table(profile));
        let write = || -> io::Result<()> {
            fs::create_dir_all(parent)?;
            let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
            temporary.write_all(document.to_string().as_bytes())?;
            temporary.as_file().sync_all()?;
            temporary.persist_noclobber(path)?;
            Ok(())
        };
        write().map_err(|error| {
            if error.kind() == io::ErrorKind::AlreadyExists {
                "A profile already exists. Restart Fliuno to open it.".into()
            } else {
                format!("Could not save profile: {error}")
            }
        })?;
        if let Some(key) = pending {
            key.commit();
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saves_a_reloadable_profile_and_does_not_replace_an_existing_config() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("config.toml");
        let key = root.path().join("test.key");
        fs::write(&key, "fixture-key\n").unwrap();
        let setup = || ModelSetup {
            provider: Provider::OpenAi,
            protocol: ModelProtocol::ChatCompletions,
            model: "org/model-\"labs\"",
            base_url: Some("http://127.0.0.1:1234/v1"),
            reasoning_effort: Some(ReasoningEffort::High),
            credential: ModelSetupCredential::File("test.key"),
        };
        Settings::create_live_model(&path, setup()).unwrap();
        let saved = fs::read_to_string(&path).unwrap();
        assert!(!saved.contains("fixture-key"));
        let model = Settings::load(&path).unwrap().model;
        assert_eq!(model.name.as_deref(), Some(setup().model));
        assert_eq!(model.base_url.as_deref(), Some("http://127.0.0.1:1234/v1/"));
        assert_eq!(
            model.effective_protocol().unwrap(),
            Some(ModelProtocol::ChatCompletions)
        );
        assert_eq!(model.reasoning_effort, Some(ReasoningEffort::High));
        assert_eq!(model.read_api_key().unwrap(), "fixture-key");
        assert!(Settings::create_live_model(&path, setup()).is_err());
        assert_eq!(fs::read_to_string(path).unwrap(), saved);
    }
}
