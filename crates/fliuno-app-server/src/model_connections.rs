//! Local profile connection RPC. Secrets are inputs to preview/save only.
use super::{Error, RpcError, RpcServer};
use fliuno_config::{
    ConnectionCredential, ModelAuth, ModelConnectionDraft, ModelConnectionEdit,
    ModelConnectionTarget, ModelSettings, Provider, Settings, provider_api_root,
};
use fliuno_protocol::{
    ModelCatalogPreview, ModelProfileConnectionGetParams, ModelProfileConnectionPreviewParams,
    ModelProfileConnectionSaveParams, ModelProfileConnectionTarget, ModelProfileConnectionView,
    ModelProfileCredentialInput, ModelProfileCredentialStatus,
};
use std::path::Path;
use zeroize::{Zeroize, Zeroizing};

impl RpcServer {
    pub(super) fn get_model_connection(
        &self,
        params: ModelProfileConnectionGetParams,
    ) -> Result<ModelProfileConnectionView, RpcError> {
        let (revision, model) = connection_snapshot(self.local_model_config()?, &params.target)?;
        Ok(connection_view(revision, model.as_ref()))
    }

    pub(super) fn preview_model_connection(
        &self,
        mut params: ModelProfileConnectionPreviewParams,
    ) -> Result<ModelCatalogPreview, RpcError> {
        with_credential(&mut params.connection.credential, |credential| {
            let path = self.local_model_config()?;
            let (revision, current) = connection_snapshot(path, &params.target)?;
            if params.revision != revision {
                return Err(Error::Conflict(
                    "Model configuration changed. Reload it and try again.".into(),
                )
                .into());
            }
            let draft = draft(
                &params.connection.provider,
                &params.connection.protocol,
                params.connection.base_url.as_deref(),
                credential,
            )?;
            let (model, key) = draft
                .for_preview(path, current.as_ref())
                .map_err(Error::Invalid)?;
            let key = key.map(Zeroizing::new);
            fliuno_providers::preview_model_connection(
                &model,
                key.as_ref().map(|key| key.as_str()),
                params.cursor.as_deref(),
            )
            .map_err(|error| Error::Invalid(error).into())
        })
    }

    pub(super) fn save_model_connection(
        &self,
        mut params: ModelProfileConnectionSaveParams,
    ) -> Result<ModelProfileConnectionView, RpcError> {
        with_credential(&mut params.connection.credential, |credential| {
            let path = self.local_model_config()?;
            let (revision, model) = Settings::save_model_connection(
                path,
                ModelConnectionEdit {
                    target: connection_target(&params.target),
                    revision: &params.revision,
                    model: &params.model,
                    reasoning_effort: params
                        .reasoning_effort
                        .as_deref()
                        .map(|effort| {
                            serde_json::from_value(serde_json::json!(effort))
                                .map_err(|_| Error::Invalid("Unknown reasoning effort".into()))
                        })
                        .transpose()?,
                    connection: draft(
                        &params.connection.provider,
                        &params.connection.protocol,
                        params.connection.base_url.as_deref(),
                        credential,
                    )?,
                },
                fliuno_providers::validate_live_profile,
            )
            .map_err(Error::Invalid)?;
            Ok(connection_view(revision, Some(&model)))
        })
    }
}

pub(super) fn with_credential<T>(
    input: &mut ModelProfileCredentialInput,
    callback: impl FnOnce(ConnectionCredential<'_>) -> Result<T, RpcError>,
) -> Result<T, RpcError> {
    match input {
        ModelProfileCredentialInput::Keep => callback(ConnectionCredential::Keep),
        ModelProfileCredentialInput::None => callback(ConnectionCredential::None),
        ModelProfileCredentialInput::Managed { key } => {
            let key = Zeroizing::new(std::mem::take(key));
            callback(ConnectionCredential::Managed(&key))
        }
        ModelProfileCredentialInput::File { path } => {
            if path.is_empty() || path.len() > 4096 || path.chars().any(char::is_control) {
                path.zeroize();
                return Err(Error::Invalid("Invalid API key file path".into()).into());
            }
            callback(ConnectionCredential::File(path))
        }
    }
}

fn draft<'a>(
    provider: &str,
    protocol: &str,
    base_url: Option<&'a str>,
    credential: ConnectionCredential<'a>,
) -> Result<ModelConnectionDraft<'a>, RpcError> {
    if base_url.is_some_and(|url| url.len() > 4096 || url.chars().any(char::is_control)) {
        return Err(Error::Invalid("Invalid provider base URL".into()).into());
    }
    Ok(ModelConnectionDraft {
        provider: serde_json::from_value(serde_json::json!(provider))
            .map_err(|_| Error::Invalid("Unknown model provider".into()))?,
        protocol: serde_json::from_value(serde_json::json!(protocol))
            .map_err(|_| Error::Invalid("Unknown model protocol".into()))?,
        base_url,
        credential,
    })
}

fn connection_snapshot(
    path: &Path,
    target: &ModelProfileConnectionTarget,
) -> Result<(String, Option<ModelSettings>), RpcError> {
    Settings::read_model_connection(path, connection_target(target))
        .map_err(|error| Error::Invalid(error).into())
}

pub(super) fn connection_target(
    target: &ModelProfileConnectionTarget,
) -> ModelConnectionTarget<'_> {
    match target {
        ModelProfileConnectionTarget::Base => ModelConnectionTarget::Base,
        ModelProfileConnectionTarget::Existing { profile } => {
            ModelConnectionTarget::Existing(profile)
        }
        ModelProfileConnectionTarget::New { profile } => ModelConnectionTarget::New(profile),
    }
}

fn connection_view(revision: String, model: Option<&ModelSettings>) -> ModelProfileConnectionView {
    ModelProfileConnectionView {
        revision,
        provider: model
            .filter(|model| model.provider != Provider::Demo)
            .map(|model| model.provider.name().to_owned()),
        protocol: model
            .and_then(|model| model.effective_protocol().ok().flatten())
            .map(|protocol| protocol.as_str().to_owned()),
        base_url: model
            .and_then(|model| model.base_url.as_deref())
            .and_then(|url| provider_api_root(url).ok())
            .map(Into::into),
        model: model.and_then(|model| model.name.clone()),
        reasoning_effort: model
            .and_then(|model| model.reasoning_effort)
            .map(|effort| effort.as_str().to_owned()),
        credential: match model {
            Some(model) if model.auth == ModelAuth::None => ModelProfileCredentialStatus::Keyless,
            Some(model) if model.api_key_file.is_some() || model.api_key_credential.is_some() => {
                ModelProfileCredentialStatus::Configured
            }
            _ => ModelProfileCredentialStatus::Unconfigured,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::rpc;
    use fliuno_core::Kernel;
    use serde_json::{Value, json};
    use std::{
        fs,
        io::{BufRead, BufReader, Write},
        net::TcpListener,
        thread,
        time::Duration,
    };

    fn response(server: &mut RpcServer, method: &str, params: Value) -> Value {
        server
            .handle_line(
                &json!({"jsonrpc":"2.0","id":1,"method":method,"params":params}).to_string(),
            )
            .unwrap()
    }

    #[test]
    fn local_connection_changes_preview_without_saving_and_never_return_secrets() {
        let root = std::env::temp_dir().join(format!("fliuno-connection-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&root).unwrap();
        let config = root.join("config.toml");
        let original = "[model]\nprovider = 'openai'\nname = 'original'\napi_key_file = 'credential-reference.key'\n";
        fs::write(&config, original).unwrap();
        fs::write(
            root.join("credential-reference.key"),
            "original-fixture-key",
        )
        .unwrap();
        let (name, provider) =
            fliuno_providers::from_settings(&Settings::load(&config).unwrap().model).unwrap();
        let mut server = RpcServer::with_provider(Kernel::in_memory(provider).unwrap(), name);
        server.enable_local_model_profiles(config.clone(), None);
        let mut attached = server.attach_connection().unwrap();
        for (server, local) in [(&mut server, true), (&mut attached, false)] {
            assert_eq!(
                rpc(
                    server,
                    "initialize",
                    json!({"protocolVersion":"0.1","clientName":"connection-test"})
                )["capabilities"]["modelProfileConnections"],
                local
            );
        }
        let target = json!({"type":"base"});
        let view = rpc(
            &mut server,
            "modelProfiles/getConnection",
            json!({"target":target}),
        );
        assert_eq!(view["credential"], "configured");
        assert_eq!(view["protocol"], "openai.responses");
        assert!(!view.to_string().contains("credential-reference"));
        for method in ["getConnection", "previewConnection", "saveConnection"] {
            assert_eq!(
                response(&mut attached, &format!("modelProfiles/{method}"), json!({}))["error"]["code"],
                -32601
            );
        }
        let malformed = response(
            &mut server,
            "modelProfiles/saveConnection",
            json!({"target": "unexpected-secret-value"}),
        );
        assert_eq!(malformed["error"]["code"], -32602);
        assert!(!malformed.to_string().contains("unexpected-secret-value"));

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let base = format!("http://{}/v1/", listener.local_addr().unwrap());
        let mut preview = json!({"target":target,"revision":view["revision"],"connection":{
            "provider":"openai","protocol":"chat_completions","baseUrl":base,"credential":{"type":"keep"}
        }});
        assert!(
            response(
                &mut server,
                "modelProfiles/previewConnection",
                preview.clone()
            )["error"]["message"]
                .as_str()
                .unwrap()
                .contains("connection changed")
        );
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
        preview["connection"]["credential"] =
            json!({"type":"managed","key":"replacement-fixture-key"});
        listener.set_nonblocking(false).unwrap();
        let worker = thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut reader = BufReader::new(&mut socket);
            let mut headers = String::new();
            loop {
                let mut line = String::new();
                assert!(reader.read_line(&mut line).unwrap() > 0);
                if line == "\r\n" {
                    break;
                }
                headers.push_str(&line);
            }
            assert!(headers.starts_with("GET /v1/models HTTP/1.1"));
            assert!(
                headers
                    .to_ascii_lowercase()
                    .contains("authorization: bearer replacement-fixture-key")
            );
            let body = r#"{"data":[{"id":"selected","name":"replacement-fixture-key"},{"id":"replacement-fixture-key"}]}"#;
            write!(socket, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
        });
        let result = rpc(
            &mut server,
            "modelProfiles/previewConnection",
            preview.clone(),
        );
        assert_eq!(result["page"]["models"][0]["id"], "selected");
        assert!(!result.to_string().contains("replacement-fixture-key"));
        worker.join().unwrap();
        assert_eq!(fs::read_to_string(&config).unwrap(), original);

        fs::write(root.join("new.key"), "replacement-fixture-key").unwrap();
        preview["connection"]["credential"] = json!({"type":"file","path":"new.key"});
        let save = json!({"target":target,"revision":view["revision"],"model":"selected","connection":preview["connection"]});
        let saved = rpc(&mut server, "modelProfiles/saveConnection", save.clone());
        assert_eq!(saved["model"], "selected");
        assert!(!saved.to_string().contains("replacement-fixture-key"));
        assert!(
            !fs::read_to_string(&config)
                .unwrap()
                .contains("replacement-fixture-key")
        );
        assert_eq!(
            Settings::load(&config)
                .unwrap()
                .model
                .read_api_key()
                .unwrap(),
            "replacement-fixture-key"
        );
        assert_eq!(
            rpc(&mut server, "modelProfiles/list", json!({}))["active"]["model"],
            "original"
        );
        assert!(
            response(&mut server, "modelProfiles/saveConnection", save)["error"]["message"]
                .as_str()
                .unwrap()
                .contains("changed")
        );
        fs::remove_dir_all(root).unwrap();
    }
}
