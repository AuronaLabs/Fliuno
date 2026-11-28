use super::*;
use std::fs;

const LEGACY: &str = r#"# User settings
[workbench]
model_profile = "fast"
[model]
provider = "anthropic"
name = "other"
[models.fast]
provider = "openai"
protocol = "responses"
name = "original"
base_url = "https://legacy.example/v1/"
api_key_file = "old.key"
[models.fast.budget]
window = 2048
max_output_tokens = 256
"#;

#[test]
fn creates_two_suppliers_and_rebinds_legacy_models_without_losing_their_settings() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    fs::write(&path, LEGACY).unwrap();
    fs::write(dir.path().join("new.key"), "replacement-key").unwrap();
    let (rev, _) = Settings::read_for_supplier_edit(&path).unwrap();
    let rev = Settings::save_supplier(
        &path,
        SupplierEdit {
            target: SupplierTarget::New("cloud"),
            revision: &rev,
            preset: Some(SupplierPreset::OpenAi),
            base_url: None,
            credential: ConnectionCredential::File("new.key"),
        },
    )
    .unwrap();
    let (rev, bound) = Settings::save_model_supplier(
        &path,
        ModelSupplierEdit {
            target: ModelConnectionTarget::Existing("fast"),
            revision: &rev,
            supplier: "cloud",
            protocol: ModelProtocol::Responses,
            model: "updated",
            reasoning_effort: Some(ReasoningEffort::High),
        },
        |model| {
            assert_eq!(model.read_model_key()?.as_deref(), Some("replacement-key"));
            Ok(())
        },
    )
    .unwrap();
    assert_eq!(
        bound.base_url.as_deref(),
        SupplierPreset::OpenAi.default_root()
    );
    assert!(matches!(
        bound.budget.unwrap().window,
        crate::ContextWindow::Tokens(2048)
    ));
    let (rev, _) = Settings::save_model_supplier(
        &path,
        ModelSupplierEdit {
            target: ModelConnectionTarget::New("quick"),
            revision: &rev,
            supplier: "cloud",
            protocol: ModelProtocol::ChatCompletions,
            model: "quick-model",
            reasoning_effort: None,
        },
        |_| Ok(()),
    )
    .unwrap();
    let rev = Settings::save_supplier(
        &path,
        SupplierEdit {
            target: SupplierTarget::New("local"),
            revision: &rev,
            preset: Some(SupplierPreset::Custom),
            base_url: Some("http://localhost:1234/v1"),
            credential: ConnectionCredential::None,
        },
    )
    .unwrap();
    let bind_local = |protocol| ModelSupplierEdit {
        target: ModelConnectionTarget::New("offline"),
        revision: &rev,
        supplier: "local",
        protocol,
        model: "local-model",
        reasoning_effort: None,
    };
    let before = fs::read(&path).unwrap();
    assert!(
        Settings::save_model_supplier(&path, bind_local(ModelProtocol::Messages), |_| Ok(()))
            .is_err()
    );
    assert!(
        Settings::save_model_supplier(&path, bind_local(ModelProtocol::ChatCompletions), |_| Err(
            "adapter refused".into()
        ))
        .is_err()
    );
    assert_eq!(fs::read(&path).unwrap(), before);
    Settings::save_model_supplier(
        &path,
        bind_local(ModelProtocol::ChatCompletions),
        |_| Ok(()),
    )
    .unwrap();
    let reopened = Settings::load(&path).unwrap();
    let active = reopened.model_for_launch(None).unwrap();
    assert_eq!(active.supplier_id(), Some("cloud"));
    assert_eq!(active.name.as_deref(), Some("updated"));
    assert_eq!(active.reasoning_effort, Some(ReasoningEffort::High));
    assert_eq!(
        active.api_key_file,
        reopened.named_model("quick").unwrap().api_key_file
    );
    assert_eq!(
        reopened
            .named_model("offline")
            .unwrap()
            .read_model_key()
            .unwrap(),
        None
    );
    assert_eq!(reopened.model.name.as_deref(), Some("other"));
    let saved = fs::read_to_string(path).unwrap();
    for original in [
        "# User settings",
        "[model]\nprovider = \"anthropic\"",
        "window = 2048",
    ] {
        assert!(saved.contains(original));
    }
    assert!(!saved.contains("old.key"));
    assert!(!saved.contains("replacement-key"));
}

#[test]
fn editing_a_shared_endpoint_requires_fresh_credentials_and_checks_every_binding() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    let original = r#"[providers.shared]
base_url = "https://before.example/v1"
api_key_file = "before.key"
[models.responses]
supplier = "shared"
protocol = "openai.responses"
name = "a"
[models.compatible]
supplier = "shared"
protocol = "openai.compatible"
name = "b"
"#;
    fs::write(&path, original).unwrap();
    fs::write(dir.path().join("before.key"), "previous-key").unwrap();
    fs::write(dir.path().join("after.key"), "new-key").unwrap();
    let (rev, _) = Settings::read_for_supplier_edit(&path).unwrap();
    let change = |base_url, credential| SupplierEdit {
        target: SupplierTarget::Existing("shared"),
        revision: &rev,
        preset: None,
        base_url: Some(base_url),
        credential,
    };
    for choice in [
        change("https://after.example/v1", ConnectionCredential::Keep),
        change("http://127.0.0.1:1234/v1", ConnectionCredential::None),
        change(
            "https://after.example/v1",
            ConnectionCredential::File("missing.key"),
        ),
    ] {
        assert!(Settings::save_supplier(&path, choice).is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), original);
    }
    let next = Settings::save_supplier(
        &path,
        change(
            "https://after.example/v1",
            ConnectionCredential::File("after.key"),
        ),
    )
    .unwrap();
    assert_ne!(next, rev);
    assert!(
        Settings::save_supplier(
            &path,
            change("https://after.example/v1", ConnectionCredential::Keep),
        )
        .unwrap_err()
        .contains("changed")
    );
    let reopened = Settings::load(&path).unwrap();
    for id in ["responses", "compatible"] {
        let model = reopened.named_model(id).unwrap();
        assert_eq!(model.base_url.as_deref(), Some("https://after.example/v1/"));
        assert_eq!(model.read_model_key().unwrap().as_deref(), Some("new-key"));
    }
}
