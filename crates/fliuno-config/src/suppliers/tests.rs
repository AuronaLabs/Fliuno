use super::*;
use crate::{
    ConnectionCredential, ModelConnectionDraft, ModelConnectionEdit, ModelConnectionTarget,
    ModelProtocol,
};
use std::fs;

const CONFIG: &str = r#"
[workbench]
model_profile = "small"
[providers.fliuno-gpt]
preset = "openai"
api_key_file = "test.key"
[providers.local]
preset = "custom"
base_url = "http://127.0.0.1:1337/v1"
auth = "none"
[model]
provider = "anthropic"
protocol = "messages"
name = "legacy"
[models.small]
supplier = "fliuno-gpt"
protocol = "openai.responses"
name = "small-model"
[models.fast]
supplier = "fliuno-gpt"
protocol = "openai.compatible"
name = "fast-model"
[models.offline]
supplier = "local"
protocol = "openai.compatible"
name = "local-model"
"#;

#[test]
fn two_suppliers_and_legacy_models_resolve_without_rewriting_the_configuration() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    fs::write(dir.path().join("test.key"), "fixture-key\n").unwrap();
    fs::write(&path, CONFIG).unwrap();
    let settings = Settings::load(&path).unwrap();
    let default = settings.model_for_launch(None).unwrap();
    let fast = settings.named_model("fast").unwrap();
    let offline = settings.named_model("offline").unwrap();
    assert_eq!(default.supplier_id(), Some("fliuno-gpt"));
    assert_eq!(default.name.as_deref(), Some("small-model"));
    assert_eq!(
        default.effective_protocol().unwrap(),
        Some(ModelProtocol::Responses)
    );
    assert_eq!(fast.supplier_id(), default.supplier_id());
    assert_eq!(
        fast.effective_protocol().unwrap(),
        Some(ModelProtocol::ChatCompletions)
    );
    assert_eq!(fast.base_url.as_deref(), Some("https://api.openai.com/v1/"));
    assert_eq!(
        fast.read_model_key().unwrap().as_deref(),
        Some("fixture-key")
    );
    assert_eq!(fast.api_key_file, default.api_key_file);
    assert_eq!(offline.supplier_id(), Some("local"));
    assert_eq!(
        offline.base_url.as_deref(),
        Some("http://127.0.0.1:1337/v1/")
    );
    assert_eq!(offline.read_model_key().unwrap(), None);
    assert_eq!(settings.model.supplier_id(), Some("anthropic"));
    assert_eq!(
        settings
            .model
            .effective_protocol()
            .unwrap()
            .unwrap()
            .as_str(),
        "claude.message"
    );
    assert_eq!(fs::read_to_string(path).unwrap(), CONFIG);
}

#[test]
fn rejected_config_cannot_inherit_a_key_from_the_wrong_connection() {
    let path = Path::new("config.toml");
    for invalid in [
        CONFIG.replace("supplier = \"local\"", "supplier = \"missing\""),
        CONFIG.replace(
            "supplier = \"local\"",
            "supplier = \"local\"\napi_key_file = \"test.key\"",
        ),
        CONFIG.replace("http://127.0.0.1:1337/v1", "https://example.com/v1"),
        CONFIG.replace("protocol = \"openai.responses\"", "provider = \"openai\""),
        CONFIG.replace("preset = \"openai\"", "preset = \"custom\""),
    ] {
        assert!(
            Settings::parse(path, &invalid).is_err(),
            "invalid config passed: {invalid}"
        );
    }
}

#[test]
fn legacy_connection_editor_preserves_the_named_binding_and_rejects_credential_rotation() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    fs::write(dir.path().join("test.key"), "fixture-key\n").unwrap();
    fs::write(&path, CONFIG).unwrap();
    let target = || ModelConnectionTarget::Existing("fast");
    let (revision, _) = Settings::read_model_connection(&path, target()).unwrap();
    let change = |credential, base_url| ModelConnectionEdit {
        target: target(),
        revision: &revision,
        connection: ModelConnectionDraft {
            provider: Provider::OpenAi,
            protocol: ModelProtocol::ChatCompletions,
            base_url,
            credential,
        },
        model: "fast-model-v2",
        reasoning_effort: None,
    };
    for change in [
        change(ConnectionCredential::File("test.key"), None),
        change(
            ConnectionCredential::Keep,
            Some("https://other.example/v1/"),
        ),
    ] {
        assert!(Settings::save_model_connection(&path, change, |_| Ok(())).is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), CONFIG);
    }
    let (_, model) = Settings::save_model_connection(
        &path,
        change(ConnectionCredential::Keep, None),
        |_| Ok(()),
    )
    .unwrap();
    assert_eq!(model.supplier_id(), Some("fliuno-gpt"));
    assert_eq!(model.name.as_deref(), Some("fast-model-v2"));
    let edited = fs::read_to_string(&path).unwrap();
    assert_eq!(
        edited,
        CONFIG.replace("name = \"fast-model\"", "name = \"fast-model-v2\"")
    );
}
