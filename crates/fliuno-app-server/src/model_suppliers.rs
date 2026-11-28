//! Model suppliers may only be inspected or edited by the configured local host.
use super::{
    Error, RpcError, RpcServer,
    model_connections::{connection_target, with_credential},
};
use fliuno_config::{
    ModelAuth, ModelSupplierEdit, ReasoningEffort, Settings, SupplierEdit, SupplierPreset,
    SupplierTarget,
};
use fliuno_protocol::{
    ModelProfileCredentialStatus, ModelSupplierBindingParams, ModelSupplierEntry,
    ModelSupplierPreset, ModelSupplierProfile, ModelSupplierRevision, ModelSupplierSaveParams,
    ModelSupplierSnapshot, ModelSupplierTarget,
};

impl RpcServer {
    pub(super) fn list_model_suppliers(&self) -> Result<ModelSupplierSnapshot, RpcError> {
        let (revision, settings) =
            Settings::read_for_supplier_edit(self.local_model_config()?).map_err(Error::Invalid)?;
        let suppliers = settings
            .providers
            .into_iter()
            .map(|(id, settings)| {
                let base_url = settings.base_url.ok_or_else(|| {
                    Error::Invalid("The supplier does not have a Base URL".into())
                })?;
                Ok(ModelSupplierEntry {
                    id,
                    base_url,
                    preset: settings.preset.map(to_wire_preset),
                    credential: match settings.auth {
                        ModelAuth::None => ModelProfileCredentialStatus::Keyless,
                        ModelAuth::ApiKey
                            if settings.api_key_file.is_some()
                                || settings.api_key_credential.is_some() =>
                        {
                            ModelProfileCredentialStatus::Configured
                        }
                        ModelAuth::ApiKey => ModelProfileCredentialStatus::Unconfigured,
                    },
                })
            })
            .collect::<Result<Vec<_>, Error>>()?;
        let models = std::iter::once((None, settings.model))
            .chain(
                settings
                    .models
                    .into_iter()
                    .map(|(id, model)| (Some(id), model)),
            )
            .map(|(profile, model)| {
                let protocol = model
                    .effective_protocol()
                    .ok()
                    .flatten()
                    .map(|protocol| protocol.as_str().to_owned());
                ModelSupplierProfile {
                    profile,
                    supplier: model.supplier,
                    model: model.name,
                    protocol,
                    reasoning_effort: model
                        .reasoning_effort
                        .map(ReasoningEffort::as_str)
                        .map(str::to_owned),
                }
            })
            .collect();
        Ok(ModelSupplierSnapshot {
            revision,
            suppliers,
            models,
        })
    }

    pub(super) fn save_model_supplier(
        &self,
        mut params: ModelSupplierSaveParams,
    ) -> Result<ModelSupplierRevision, RpcError> {
        with_credential(&mut params.credential, |credential| {
            if params
                .base_url
                .as_deref()
                .is_some_and(|url| url.len() > 4096 || url.chars().any(char::is_control))
            {
                return Err(Error::Invalid("Invalid supplier Base URL".into()).into());
            }
            let target = match &params.target {
                ModelSupplierTarget::New { supplier } => SupplierTarget::New(supplier),
                ModelSupplierTarget::Existing { supplier } => SupplierTarget::Existing(supplier),
            };
            let revision = Settings::save_supplier(
                self.local_model_config()?,
                SupplierEdit {
                    target,
                    revision: &params.revision,
                    preset: params.preset.map(from_wire_preset),
                    base_url: params.base_url.as_deref(),
                    credential,
                },
            )
            .map_err(Error::Invalid)?;
            Ok(ModelSupplierRevision { revision })
        })
    }

    pub(super) fn bind_model_supplier(
        &self,
        params: ModelSupplierBindingParams,
    ) -> Result<ModelSupplierRevision, RpcError> {
        let protocol = serde_json::from_value(serde_json::Value::String(params.protocol))
            .map_err(|_| Error::Invalid("Unknown model protocol".into()))?;
        let reasoning_effort = params
            .reasoning_effort
            .map(|value| {
                serde_json::from_value(serde_json::Value::String(value))
                    .map_err(|_| Error::Invalid("Unknown reasoning effort".into()))
            })
            .transpose()?;
        let (revision, _) = Settings::save_model_supplier(
            self.local_model_config()?,
            ModelSupplierEdit {
                target: connection_target(&params.target),
                revision: &params.revision,
                supplier: &params.supplier,
                protocol,
                model: &params.model,
                reasoning_effort,
            },
            fliuno_providers::validate_live_profile,
        )
        .map_err(Error::Invalid)?;
        Ok(ModelSupplierRevision { revision })
    }
}

fn to_wire_preset(preset: SupplierPreset) -> ModelSupplierPreset {
    match preset {
        SupplierPreset::OpenAi => ModelSupplierPreset::OpenAi,
        SupplierPreset::Anthropic => ModelSupplierPreset::Anthropic,
        SupplierPreset::Gemini => ModelSupplierPreset::Gemini,
        SupplierPreset::Custom => ModelSupplierPreset::Custom,
    }
}

fn from_wire_preset(preset: ModelSupplierPreset) -> SupplierPreset {
    match preset {
        ModelSupplierPreset::OpenAi => SupplierPreset::OpenAi,
        ModelSupplierPreset::Anthropic => SupplierPreset::Anthropic,
        ModelSupplierPreset::Gemini => SupplierPreset::Gemini,
        ModelSupplierPreset::Custom => SupplierPreset::Custom,
    }
}

#[cfg(test)]
mod tests;
