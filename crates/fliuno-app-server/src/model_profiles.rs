//! Profiles available to the trusted local stdio host.
use super::{Error, RpcError, RpcServer};
use fliuno_config::{ModelSettings, Provider, Settings};
use fliuno_protocol::{
    ModelConnectionCheck, ModelProfileEntry, ModelProfileList, ProviderModelListParams,
    ProviderModelPage,
};
use std::path::PathBuf;

pub(super) struct Catalog {
    path: PathBuf,
    active: Option<String>,
}

impl Catalog {
    fn model_for_lookup<'a>(
        &self,
        settings: &'a Settings,
        profile: Option<&str>,
    ) -> Result<&'a ModelSettings, Error> {
        if let Some(id) = profile.or(self.active.as_deref()) {
            settings.model_for_launch(Some(id)).map_err(Error::Invalid)
        } else {
            Ok(&settings.model)
        }
    }
}

impl RpcServer {
    /// The host supplies the trusted settings file. Wire requests only select a
    /// profile in that file. Other attached connections have no profile control.
    pub fn enable_local_model_profiles(&mut self, path: PathBuf, active: Option<String>) {
        self.model_profiles = Some(Catalog { path, active });
    }

    fn model_profile_catalog(&self) -> Result<&Catalog, RpcError> {
        self.model_profiles.as_ref().ok_or_else(|| RpcError {
            code: -32601,
            message: "Model profiles are unavailable on this connection".into(),
        })
    }

    fn active_model_profile(&self, profile: Option<String>) -> Result<ModelProfileEntry, RpcError> {
        let model = self
            .kernel()?
            .configured_model_capabilities()
            .ok_or_else(|| Error::Conflict("The active provider has no model identity".into()))?;
        Ok(ModelProfileEntry {
            profile,
            provider: model.provider,
            model: model.model,
            reasoning_effort: model.reasoning_effort,
        })
    }

    pub(super) fn list_model_profiles(&self) -> Result<ModelProfileList, RpcError> {
        let catalog = self.model_profile_catalog()?;
        let settings = Settings::load(&catalog.path).map_err(Error::Invalid)?;
        let active = self.active_model_profile(catalog.active.clone())?;
        let profiles = std::iter::once((None, &settings.model))
            .chain(
                settings
                    .models
                    .iter()
                    .map(|(id, model)| (Some(id.as_str()), model)),
            )
            .filter(|(_, model)| !matches!(model.provider, Provider::Demo))
            .map(|(id, model)| {
                if id == active.profile.as_deref() {
                    active.clone()
                } else {
                    entry(id, model)
                }
            })
            .collect();
        Ok(ModelProfileList { active, profiles })
    }

    pub(super) fn check_provider_model(
        &self,
        profile: Option<&str>,
    ) -> Result<ModelConnectionCheck, RpcError> {
        let catalog = self.model_profile_catalog()?;
        let settings = Settings::load(&catalog.path).map_err(Error::Invalid)?;
        let model = catalog.model_for_lookup(&settings, profile)?;
        Ok(fliuno_providers::check_model_connection(model))
    }

    pub(super) fn discover_provider_models(
        &self,
        params: ProviderModelListParams,
    ) -> Result<ProviderModelPage, RpcError> {
        let catalog = self.model_profile_catalog()?;
        let settings = Settings::load(&catalog.path).map_err(Error::Invalid)?;
        let model = catalog.model_for_lookup(&settings, params.profile.as_deref())?;
        fliuno_providers::discover_models(model, params.cursor.as_deref())
            .map_err(|error| Error::Invalid(error).into())
    }

    pub(super) fn select_model_profile(
        &mut self,
        profile: Option<String>,
    ) -> Result<ModelProfileEntry, RpcError> {
        let catalog = self.model_profile_catalog()?;
        if profile == catalog.active {
            return self.active_model_profile(profile);
        }
        let settings = Settings::load(&catalog.path).map_err(Error::Invalid)?;
        let model = if let Some(id) = profile.as_deref() {
            settings
                .model_for_launch(Some(id))
                .map_err(Error::Invalid)?
        } else {
            &settings.model
        };
        if matches!(model.provider, Provider::Demo) {
            return Err(Error::Invalid("Choose a configured live model".into()).into());
        }
        self.kernel()?.require_model_switch_ready()?;
        let (provider_name, provider) =
            fliuno_providers::from_settings(model).map_err(Error::Invalid)?;
        self.kernel()?.replace_model_provider(provider)?;
        self.provider_name = provider_name;
        if let Some(catalog) = &mut self.model_profiles {
            catalog.active = profile.clone();
        }
        self.active_model_profile(profile)
    }
}

fn entry(id: Option<&str>, model: &ModelSettings) -> ModelProfileEntry {
    ModelProfileEntry {
        profile: id.map(str::to_owned),
        provider: model.runtime_provider_name().to_owned(),
        model: model.name.clone().unwrap_or_default(),
        reasoning_effort: model
            .reasoning_effort
            .map(|effort| effort.as_str().to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::rpc;
    use fliuno_core::Kernel;
    use serde_json::{Value, json};

    fn failure(server: &mut RpcServer, method: &str, params: Value) -> Value {
        let reply = server
            .handle_line(
                &json!({"jsonrpc":"2.0","id":1,"method":method,"params":params}).to_string(),
            )
            .unwrap();
        assert!(reply.get("result").is_none(), "{reply}");
        reply["error"].clone()
    }

    #[test]
    fn switches_only_local_configured_profiles_after_other_workspaces_settle() {
        let root =
            std::env::temp_dir().join(format!("fliuno-model-profiles-{}", uuid::Uuid::new_v4()));
        let original = root.join("original");
        let other = root.join("other");
        std::fs::create_dir_all(&original).unwrap();
        std::fs::create_dir(&other).unwrap();
        let config = root.join("config.toml");
        let settings = r#"
[model]
provider = "openai"
name = "default-model"
api_key_file = "valid.key"
[models.gemini]
provider = "gemini"
name = "alternate-model"
api_key_file = "valid.key"
[models.invalid]
provider = "openai"
name = "invalid-model"
api_key_file = "missing.key"
"#;
        std::fs::write(&config, settings).unwrap();
        std::fs::write(root.join("valid.key"), "test-key\n").unwrap();
        let settings = Settings::load(&config).unwrap();
        let (name, provider) = fliuno_providers::from_settings(&settings.model).unwrap();
        let kernel = Kernel::in_memory(provider).unwrap();
        let mut server = RpcServer::with_provider(kernel, name);
        server.enable_local_model_profiles(config, None);
        let initialize = json!({"protocolVersion":"0.1","clientName":"profile-contract-test"});
        let info = rpc(&mut server, "initialize", initialize.clone());
        assert_eq!(info["capabilities"]["modelProfiles"], true);
        let mut attached = server.attach_connection().unwrap();
        rpc(&mut server, "workspace/open", json!({"root":original}));
        let task = {
            let mut kernel = server.kernel().unwrap();
            let workspace = kernel.open_workspace(other.to_str().unwrap()).unwrap();
            let session = kernel.create_session(&workspace.id).unwrap();
            kernel
                .create_task_with_key(&session.id, "pending elsewhere", None)
                .unwrap()
                .0
        };
        assert!(
            failure(
                &mut server,
                "modelProfiles/select",
                json!({"profile":"gemini"})
            )["message"]
                .as_str()
                .unwrap()
                .contains("existing model work")
        );
        let check = rpc(&mut server, "models/check", json!({"profile":"invalid"}));
        assert_eq!(check["configuration"], "ready");
        assert_eq!(check["credentials"], "unavailable");
        let list = rpc(&mut server, "modelProfiles/list", json!({}));
        assert_eq!(list["active"]["model"], "default-model");
        assert_eq!(
            rpc(&mut server, "task/get", json!({"taskId":task.id}))["status"],
            "running"
        );
        rpc(&mut server, "task/cancel", json!({"taskId":task.id}));
        failure(
            &mut server,
            "modelProfiles/select",
            json!({"profile":"invalid"}),
        );
        failure(
            &mut server,
            "modelProfiles/select",
            json!({"profile":"nonexistent"}),
        );
        assert_eq!(
            rpc(&mut server, "modelProfiles/list", json!({}))["active"]["model"],
            "default-model"
        );
        let selected = rpc(
            &mut server,
            "modelProfiles/select",
            json!({"profile":"gemini"}),
        );
        assert_eq!(
            selected,
            json!({"profile":"gemini", "provider":"gemini", "model":"alternate-model"})
        );
        failure(&mut server, "modelProfiles/select", json!({}));
        assert_eq!(
            rpc(&mut server, "modelProfiles/list", json!({}))["active"],
            selected
        );
        let observed = rpc(&mut attached, "initialize", initialize);
        assert_eq!(observed["capabilities"]["modelProfiles"], false);
        assert_eq!(observed["capabilities"]["provider"], "gemini");
        assert_eq!(
            failure(&mut attached, "models/list", json!({}))["code"],
            -32601
        );
        assert_eq!(
            failure(&mut attached, "models/check", json!({}))["code"],
            -32601
        );
        assert_eq!(
            failure(&mut attached, "modelProfiles/list", json!({}))["code"],
            -32601
        );
        assert_eq!(
            failure(
                &mut attached,
                "modelProfiles/select",
                json!({"profile":null})
            )["code"],
            -32601
        );
        let selected = rpc(&mut server, "modelProfiles/select", json!({"profile":null}));
        assert_eq!(selected["profile"], Value::Null);
        assert_eq!(selected["model"], "default-model");
        drop(attached);
        drop(server);
        std::fs::remove_dir_all(root).unwrap();
    }
}
