//! Shared rules for first-run setup and editing an existing connection.
use super::{
    ModelAuth, ModelProtocol, ModelSettings, PendingApiKey, Provider, ReasoningEffort, Settings,
    config_edit::{ConfigEdit, replace_value, revision},
    model_edit::validate_model_name,
    provider_api_root, read_api_key, validate_api_key, validate_profile_id,
};
use std::path::Path;
use toml_edit::{DocumentMut, Item, Table, TableLike, value};

pub struct ModelConnectionDraft<'a> {
    pub provider: Provider,
    pub protocol: ModelProtocol,
    pub base_url: Option<&'a str>,
    pub credential: ConnectionCredential<'a>,
}

pub enum ConnectionCredential<'a> {
    Keep,
    Managed(&'a str),
    File(&'a str),
    None,
}

pub struct ModelConnectionEdit<'a> {
    pub target: ModelConnectionTarget<'a>,
    pub revision: &'a str,
    pub model: &'a str,
    pub reasoning_effort: Option<ReasoningEffort>,
    pub connection: ModelConnectionDraft<'a>,
}

pub enum ModelConnectionTarget<'a> {
    Base,
    Existing(&'a str),
    New(&'a str),
}

impl ModelConnectionDraft<'_> {
    fn validate(&self, current: Option<&ModelSettings>) -> Result<ModelSettings, String> {
        let base_url = self
            .base_url
            .map(provider_api_root)
            .transpose()?
            .map(|url| url.into());
        let auth = match self.credential {
            ConnectionCredential::Keep => {
                let old = current.ok_or("Choose a credential for the new profile")?;
                let new_root = provider_api_root(self.provider.api_root(self.base_url))?;
                let same_route = old.provider == self.provider
                    && old.effective_protocol().ok().flatten() == Some(self.protocol)
                    && provider_api_root(old.provider.api_root(old.base_url.as_deref()))
                        .is_ok_and(|root| root == new_root);
                if !same_route {
                    return Err("The connection changed. Choose credentials for the new endpoint and protocol.".into());
                }
                old.auth
            }
            ConnectionCredential::None => ModelAuth::None,
            ConnectionCredential::File(_) | ConnectionCredential::Managed(_) => ModelAuth::ApiKey,
        };
        let model = ModelSettings {
            provider: self.provider,
            protocol: Some(self.protocol),
            base_url,
            auth,
            ..Default::default()
        };
        if model.effective_protocol()?.is_none() {
            return Err("Choose a live model protocol".into());
        }
        model.validate_model_auth()?;
        Ok(model)
    }

    /// Validate the draft and resolve its key without writing a credential or profile.
    pub fn for_preview(
        &self,
        config: &Path,
        current: Option<&ModelSettings>,
    ) -> Result<(ModelSettings, Option<String>), String> {
        let model = self.validate(current)?;
        let key = match self.credential {
            ConnectionCredential::Keep => current
                .ok_or("Choose a credential for the new profile")?
                .read_model_key()?,
            ConnectionCredential::None => None,
            ConnectionCredential::File(file) => Some(read_api_key(&key_file(config, file))?),
            ConnectionCredential::Managed(key) => {
                validate_api_key(key)?;
                Some(key.to_owned())
            }
        };
        Ok((model, key))
    }

    pub(super) fn stage(
        &self,
        path: &Path,
        table: &mut dyn TableLike,
        current: Option<&ModelSettings>,
    ) -> Result<Option<PendingApiKey>, String> {
        let model = self.validate(current)?;
        replace_value(table, "provider", value(self.provider.name()));
        replace_value(table, "protocol", value(self.protocol.as_str()));
        if let Some(base_url) = model.base_url {
            replace_value(table, "base_url", value(base_url));
        } else {
            table.remove("base_url");
        }
        match self.credential {
            ConnectionCredential::Keep => Ok(None),
            ConnectionCredential::None => {
                table.remove("api_key_file");
                table.remove("api_key_credential");
                replace_value(table, "auth", value("none"));
                Ok(None)
            }
            ConnectionCredential::File(file) => {
                read_api_key(&key_file(path, file))?;
                table.remove("auth");
                table.remove("api_key_credential");
                replace_value(table, "api_key_file", value(file));
                Ok(None)
            }
            ConnectionCredential::Managed(key) => {
                let pending = PendingApiKey::stage(path, key)?;
                let reference = pending
                    .setting()
                    .parse::<DocumentMut>()
                    .map_err(|_| "Could not write the credential reference")?;
                table.remove("auth");
                for field in ["api_key_file", "api_key_credential"] {
                    if let Some(item) = reference.get(field) {
                        replace_value(table, field, item.clone());
                    } else {
                        table.remove(field);
                    }
                }
                Ok(Some(pending))
            }
        }
    }
}

fn key_file(config: &Path, file: &str) -> std::path::PathBuf {
    config.parent().unwrap_or_else(|| Path::new("")).join(file)
}

impl Settings {
    /// Read a config revision even when there is no existing live profile to edit.
    pub fn model_connection_revision(path: &Path) -> Result<String, String> {
        let text = super::read_file(path, super::config_edit::CONFIG_LIMIT)
            .map_err(|_| "Cannot read model configuration")?;
        Self::parse(path, &text)?;
        Ok(revision(&text))
    }
}

impl Settings {
    /// Add a named connection or replace the connection of an existing profile.
    pub fn save_model_connection(
        path: &Path,
        change: ModelConnectionEdit<'_>,
        validate: impl FnOnce(&ModelSettings) -> Result<(), String>,
    ) -> Result<(String, ModelSettings), String> {
        let edit = ConfigEdit::open(path)?;
        edit.require_revision(change.revision)?;
        let settings = edit.settings()?;
        let (id, old) = match change.target {
            ModelConnectionTarget::New(id) => {
                validate_profile_id(id)?;
                if settings.models.contains_key(id) {
                    return Err("A model profile already uses this name".into());
                }
                (Some(id), None)
            }
            ModelConnectionTarget::Existing(id) => (Some(id), Some(settings.named_model(id)?)),
            ModelConnectionTarget::Base => (None, Some(&settings.model)),
        };
        let model_name = validate_model_name(change.model)?;
        let mut doc = edit.document()?;
        let table = match change.target {
            ModelConnectionTarget::New(id) => {
                let models = doc
                    .entry("models")
                    .or_insert(Item::Table(Table::new()))
                    .as_table_like_mut()
                    .ok_or("Invalid model profiles")?;
                models.insert(id, Item::Table(Table::new()));
                models.get_mut(id)
            }
            ModelConnectionTarget::Existing(id) => doc
                .get_mut("models")
                .and_then(Item::as_table_like_mut)
                .and_then(|models| models.get_mut(id)),
            ModelConnectionTarget::Base => doc.get_mut("model"),
        }
        .and_then(Item::as_table_like_mut)
        .ok_or("Model profile is missing")?;
        let pending = change.connection.stage(path, table, old)?;
        replace_value(table, "name", value(model_name));
        if let Some(effort) = change.reasoning_effort {
            replace_value(table, "reasoning_effort", value(effort.as_str()));
        } else {
            table.remove("reasoning_effort");
        }
        let output = doc.to_string();
        let mut settings = edit.parse(&output)?;
        let model = match id {
            Some(id) => settings
                .models
                .remove(id)
                .ok_or("Model profile is missing")?,
            None => settings.model,
        };
        model.validate_reasoning_effort()?;
        model.validate_budget()?;
        model.read_model_key()?;
        validate(&model)?;
        edit.commit(&output)?;
        if let Some(key) = pending {
            key.commit();
        }
        Ok((revision(&output), model))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn preview_and_save_require_a_fresh_credential_choice_for_a_new_route() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        let original = "# leave this alone\n[model]\nprovider = 'openai'\nname = 'before'\napi_key_file = 'old.key'\n[workbench]\ntheme = 'dark'\n";
        fs::write(&path, original).unwrap();
        fs::write(directory.path().join("old.key"), "fixture-old").unwrap();
        let (revision, current) = Settings::read_model_for_edit(&path, None).unwrap();
        for (base_url, protocol) in [
            (
                Some("https://api.openai.com/gateway/"),
                ModelProtocol::Responses,
            ),
            (Some("https://example.com/v1/"), ModelProtocol::Responses),
            (None, ModelProtocol::ChatCompletions),
        ] {
            let draft = || ModelConnectionDraft {
                provider: Provider::OpenAi,
                protocol,
                base_url,
                credential: ConnectionCredential::Keep,
            };
            assert!(draft().for_preview(&path, Some(&current)).is_err());
            let error = Settings::save_model_connection(
                &path,
                ModelConnectionEdit {
                    target: ModelConnectionTarget::Base,
                    revision: &revision,
                    model: "after",
                    reasoning_effort: None,
                    connection: draft(),
                },
                |_| panic!("must reject before validation or dispatch"),
            )
            .unwrap_err();
            assert!(error.contains("connection changed"));
            assert_eq!(fs::read_to_string(&path).unwrap(), original);
        }
        let equivalent = ModelConnectionDraft {
            provider: Provider::OpenAi,
            protocol: ModelProtocol::Responses,
            base_url: Some("https://API.openai.com:443/v1"),
            credential: ConnectionCredential::Keep,
        };
        let (_, key) = equivalent.for_preview(&path, Some(&current)).unwrap();
        assert_eq!(key.as_deref(), Some("fixture-old"));
        assert_eq!(fs::read_to_string(&path).unwrap(), original);
    }

    #[test]
    fn failed_rotation_rolls_back_staged_key_and_a_new_profile_preserves_existing_settings() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        let original = "[model]\nprovider = 'openai'\nname = 'original'\napi_key_file = 'old.key'\n[workbench]\ntheme = 'dark'\n";
        fs::write(&path, original).unwrap();
        fs::write(directory.path().join("old.key"), "fixture-old").unwrap();
        let rev = Settings::model_connection_revision(&path).unwrap();
        let rotation = ModelConnectionEdit {
            target: ModelConnectionTarget::Base,
            revision: &rev,
            model: "replacement",
            reasoning_effort: None,
            connection: ModelConnectionDraft {
                provider: Provider::OpenAi,
                protocol: ModelProtocol::ChatCompletions,
                base_url: Some("https://example.com/v1/"),
                credential: ConnectionCredential::Managed("fixture-replacement"),
            },
        };
        let mut staged_key = None;
        let error = Settings::save_model_connection(&path, rotation, |model| {
            assert_eq!(model.read_api_key().unwrap(), "fixture-replacement");
            staged_key = Some((model.api_key_file.clone(), model.api_key_credential.clone()));
            Err("Rejected fixture".into())
        })
        .unwrap_err();
        assert_eq!(error, "Rejected fixture");
        let (api_key_file, api_key_credential) = staged_key.unwrap();
        let staged = ModelSettings {
            api_key_file,
            api_key_credential,
            ..Default::default()
        };
        assert!(staged.read_api_key().is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), original);

        let add = || ModelConnectionEdit {
            target: ModelConnectionTarget::New("local"),
            revision: &rev,
            model: "my-model",
            reasoning_effort: None,
            connection: ModelConnectionDraft {
                provider: Provider::OpenAi,
                protocol: ModelProtocol::ChatCompletions,
                base_url: Some("http://127.0.0.1:8080/v1"),
                credential: ConnectionCredential::None,
            },
        };
        let (new_revision, local) =
            Settings::save_model_connection(&path, add(), |_| Ok(())).unwrap();
        assert_ne!(new_revision, rev);
        assert!(local.read_model_key().unwrap().is_none());
        assert!(
            Settings::save_model_connection(&path, add(), |_| Ok(()))
                .unwrap_err()
                .contains("changed")
        );
        let saved = Settings::load(&path).unwrap();
        assert_eq!(saved.model.read_api_key().unwrap(), "fixture-old");
        assert_eq!(
            saved.named_model("local").unwrap().name.as_deref(),
            Some("my-model")
        );
        assert!(matches!(saved.workbench.theme, super::super::Theme::Dark));
        assert!(fs::read_to_string(&path).unwrap().starts_with(original));
    }
}
