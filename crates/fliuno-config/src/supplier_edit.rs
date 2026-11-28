//! Revision-bound writes for user-owned supplier connections and model bindings.
use super::{
    ConnectionCredential, ModelAuth, ModelConnectionTarget, ModelProtocol, ModelSettings,
    ReasoningEffort, Settings, SupplierPreset,
    config_edit::{CONFIG_LIMIT, ConfigEdit, replace_value, revision},
    is_local_provider_root,
    model_edit::validate_model_name,
    provider_api_root, read_file,
    suppliers::resolved_root,
    validate_profile_id,
};
use std::path::Path;
use toml_edit::{Item, Table, value};

#[derive(Clone, Copy)]
pub enum SupplierTarget<'a> {
    New(&'a str),
    Existing(&'a str),
}

pub struct SupplierEdit<'a> {
    pub target: SupplierTarget<'a>,
    pub revision: &'a str,
    pub preset: Option<SupplierPreset>,
    pub base_url: Option<&'a str>,
    pub credential: ConnectionCredential<'a>,
}

pub struct ModelSupplierEdit<'a> {
    pub target: ModelConnectionTarget<'a>,
    pub revision: &'a str,
    pub supplier: &'a str,
    pub protocol: ModelProtocol,
    pub model: &'a str,
    pub reasoning_effort: Option<ReasoningEffort>,
}

impl Settings {
    /// Read settings and their revision from the same bytes. Key references are
    /// resolved, but no key values are read or returned.
    pub fn read_for_supplier_edit(path: &Path) -> Result<(String, Self), String> {
        let text = read_file(path, CONFIG_LIMIT).map_err(|_| "Cannot read model configuration")?;
        Ok((revision(&text), Self::parse(path, &text)?))
    }

    /// Save one connection without changing the models bound to its ID.
    pub fn save_supplier(path: &Path, change: SupplierEdit<'_>) -> Result<String, String> {
        let edit = ConfigEdit::open(path)?;
        edit.require_revision(change.revision)?;
        let settings = edit.settings()?;
        let (id, is_new) = match change.target {
            SupplierTarget::New(id) => (id, true),
            SupplierTarget::Existing(id) => (id, false),
        };
        validate_profile_id(id)?;
        let current = settings.providers.get(id);
        if is_new && current.is_some() {
            return Err("A supplier already uses this name".into());
        }
        if !is_new && current.is_none() {
            return Err("Supplier is missing".into());
        }
        let root = resolved_root(change.preset, change.base_url)?;
        let auth = match change.credential {
            ConnectionCredential::Keep => {
                let current = current.ok_or("Choose a credential for the new supplier")?;
                if current.base_url.as_deref() != Some(root.as_str()) {
                    return Err(
                        "The Base URL changed. Choose credentials for the new endpoint.".into(),
                    );
                }
                current.auth
            }
            ConnectionCredential::None => ModelAuth::None,
            _ => ModelAuth::ApiKey,
        };
        if auth == ModelAuth::None && !is_local_provider_root(&provider_api_root(&root)?) {
            return Err("Keyless model connections require a loopback Base URL".into());
        }
        let mut doc = edit.document()?;
        let providers = doc
            .entry("providers")
            .or_insert(Item::Table(Table::new()))
            .as_table_like_mut()
            .ok_or("Invalid suppliers")?;
        if is_new {
            providers.insert(id, Item::Table(Table::new()));
        }
        let table = providers
            .get_mut(id)
            .and_then(Item::as_table_like_mut)
            .ok_or("Supplier is missing")?;
        if let Some(preset) = change.preset {
            replace_value(table, "preset", value(preset.as_str()));
        } else {
            table.remove("preset");
        }
        if change.base_url.is_some() {
            replace_value(table, "base_url", value(root));
        } else {
            table.remove("base_url");
        }
        let pending = change.credential.stage(path, table)?;
        let output = doc.to_string();
        // Parsing resolves every model using this supplier. In particular, an
        // incompatible protocol must prevent a switch to keyless mode.
        let saved = edit.parse(&output)?;
        let connection = &saved.providers[id];
        if connection.auth == ModelAuth::ApiKey {
            ModelSettings {
                api_key_file: connection.api_key_file.clone(),
                api_key_credential: connection.api_key_credential.clone(),
                ..Default::default()
            }
            .read_api_key()?;
        }
        edit.commit(&output)?;
        if let Some(key) = pending {
            key.commit();
        }
        Ok(revision(&output))
    }

    /// Add or rebind one model, keeping its other fields and default selection.
    pub fn save_model_supplier(
        path: &Path,
        change: ModelSupplierEdit<'_>,
        validate: impl FnOnce(&ModelSettings) -> Result<(), String>,
    ) -> Result<(String, ModelSettings), String> {
        let edit = ConfigEdit::open(path)?;
        edit.require_revision(change.revision)?;
        let settings = edit.settings()?;
        change.target.current(&settings)?;
        validate_profile_id(change.supplier)?;
        if !settings.providers.contains_key(change.supplier) {
            return Err("Supplier is missing".into());
        }
        let name = validate_model_name(change.model)?;
        let mut doc = edit.document()?;
        let table = change.target.table(&mut doc)?;
        for field in [
            "provider",
            "base_url",
            "auth",
            "api_key_file",
            "api_key_credential",
        ] {
            table.remove(field);
        }
        replace_value(table, "supplier", value(change.supplier));
        replace_value(table, "protocol", value(change.protocol.as_str()));
        replace_value(table, "name", value(name));
        if let Some(effort) = change.reasoning_effort {
            replace_value(table, "reasoning_effort", value(effort.as_str()));
        } else {
            table.remove("reasoning_effort");
        }
        let output = doc.to_string();
        let mut settings = edit.parse(&output)?;
        let model = match change.target.profile_id() {
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
        Ok((revision(&output), model))
    }
}

#[cfg(test)]
mod tests;
