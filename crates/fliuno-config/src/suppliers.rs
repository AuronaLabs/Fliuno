//! Named, user-owned API endpoints. Resolve these once when loading the full
//! config; adapters consume the same validated settings as legacy profiles.
use super::{
    ModelAuth, ModelSettings, Provider, Settings, provider_api_root, resolve_relative,
    validate_profile_id,
};
use serde::Deserialize;
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SupplierPreset {
    OpenAi,
    Anthropic,
    Gemini,
    Custom,
}

impl SupplierPreset {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::OpenAi => "openai",
            Self::Anthropic => "anthropic",
            Self::Gemini => "gemini",
            Self::Custom => "custom",
        }
    }

    pub fn default_root(self) -> Option<&'static str> {
        Some(match self {
            Self::OpenAi => Provider::OpenAi.api_root(None),
            Self::Anthropic => Provider::Anthropic.api_root(None),
            Self::Gemini => Provider::Gemini.api_root(None),
            Self::Custom => return None,
        })
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SupplierSettings {
    pub preset: Option<SupplierPreset>,
    pub base_url: Option<String>,
    pub auth: ModelAuth,
    pub api_key_file: Option<PathBuf>,
    pub api_key_credential: Option<String>,
}

pub(super) fn resolve(settings: &mut Settings, base: &Path) -> Result<(), String> {
    for (id, supplier) in &mut settings.providers {
        validate_profile_id(id)
            .map_err(|_| "Supplier IDs use 1–48 letters, digits, hyphens or underscores")?;
        supplier.base_url = Some(
            resolved_root(supplier.preset, supplier.base_url.as_deref())
                .map_err(|error| format!("Supplier {id}: {error}"))?,
        );
        if supplier.api_key_file.is_some() && supplier.api_key_credential.is_some()
            || supplier.auth == ModelAuth::None
                && (supplier.api_key_file.is_some() || supplier.api_key_credential.is_some())
        {
            return Err(format!("Supplier {id} has conflicting credential settings"));
        }
        resolve_relative(&mut supplier.api_key_file, base);
    }
    resolve_model(&mut settings.model, &settings.providers)?;
    for model in settings.models.values_mut() {
        resolve_model(model, &settings.providers)?;
    }
    Ok(())
}

pub(super) fn resolved_root(
    preset: Option<SupplierPreset>,
    base_url: Option<&str>,
) -> Result<String, &'static str> {
    let root = base_url
        .or_else(|| preset.and_then(SupplierPreset::default_root))
        .ok_or("Choose a Base URL or a built-in preset")?;
    Ok(provider_api_root(root)?.into())
}

fn resolve_model(
    model: &mut ModelSettings,
    providers: &BTreeMap<String, SupplierSettings>,
) -> Result<(), String> {
    let Some(id) = &model.supplier else {
        return Ok(());
    };
    validate_profile_id(id).map_err(|_| "Invalid supplier ID")?;
    let supplier = providers
        .get(id)
        .ok_or_else(|| format!("Unknown supplier {id}"))?;
    if model.provider != Provider::Demo
        || model.base_url.is_some()
        || model.auth != ModelAuth::ApiKey
        || model.api_key_file.is_some()
        || model.api_key_credential.is_some()
    {
        return Err(format!(
            "Model using supplier {id} cannot also define an inline connection"
        ));
    }
    let protocol = model
        .protocol
        .ok_or_else(|| format!("Model using supplier {id} requires an explicit protocol ID"))?;
    model.provider = protocol.adapter();
    model.base_url.clone_from(&supplier.base_url);
    model.auth = supplier.auth;
    model.api_key_file.clone_from(&supplier.api_key_file);
    model
        .api_key_credential
        .clone_from(&supplier.api_key_credential);
    model
        .validate_model_auth()
        .map_err(|error| format!("Supplier {id}: {error}"))?;
    Ok(())
}

impl Settings {
    /// Form an unsaved model route using a resolved supplier in this snapshot.
    pub fn model_for_supplier_catalog(
        &self,
        supplier: String,
        protocol: super::ModelProtocol,
    ) -> Result<ModelSettings, String> {
        let mut model = ModelSettings {
            supplier: Some(supplier),
            protocol: Some(protocol),
            ..Default::default()
        };
        resolve_model(&mut model, &self.providers)?;
        Ok(model)
    }
}

impl ModelSettings {
    /// Identity for display. The wire adapter and provider replay namespace
    /// continue to be chosen by the protocol.
    pub fn supplier_id(&self) -> Option<&str> {
        self.supplier
            .as_deref()
            .or_else(|| (self.provider != Provider::Demo).then(|| self.provider.name()))
    }
}

#[cfg(test)]
mod tests;
