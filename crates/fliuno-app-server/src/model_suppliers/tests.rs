use super::*;
use crate::tests::rpc;
use fliuno_core::Kernel;
use serde_json::{Value, json};
use std::{fs, path::PathBuf};

struct ConfigFixture(PathBuf);

impl Drop for ConfigFixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn response(server: &mut RpcServer, method: &str, params: Value) -> Value {
    server
        .handle_line(&json!({"jsonrpc":"2.0","id":1,"method":method,"params":params}).to_string())
        .unwrap()
}

#[test]
fn supplier_settings_are_local_and_saving_does_not_switch_the_model() {
    let root = ConfigFixture(
        std::env::temp_dir().join(format!("fliuno-supplier-{}", uuid::Uuid::new_v4())),
    );
    fs::create_dir(&root.0).unwrap();
    let config = root.0.join("config.toml");
    fs::write(root.0.join("private-reference.key"), "fake-supplier-key").unwrap();
    fs::write(
        &config,
        r#"
[model]
provider = "openai"
protocol = "chat_completions"
name = "offline-original"
base_url = "http://127.0.0.1:1234/v1"
auth = "none"
"#,
    )
    .unwrap();
    let (name, provider) =
        fliuno_providers::from_settings(&Settings::load(&config).unwrap().model).unwrap();
    let mut local = RpcServer::with_provider(Kernel::in_memory(provider).unwrap(), name);
    local.enable_local_model_profiles(config, None);
    let mut attached = local.attach_connection().unwrap();
    for (server, enabled) in [(&mut local, true), (&mut attached, false)] {
        assert_eq!(
            rpc(
                server,
                "initialize",
                json!({"protocolVersion":"0.1","clientName":"supplier-test"})
            )["capabilities"]["modelSupplierSettings"],
            enabled
        );
    }
    for method in [
        "modelSuppliers/list",
        "modelSuppliers/save",
        "modelProfiles/bindSupplier",
    ] {
        assert_eq!(
            response(&mut attached, method, json!({}))["error"]["code"],
            -32601
        );
    }
    let initial = rpc(&mut local, "modelSuppliers/list", json!({}));
    assert_eq!(initial["suppliers"], json!([]));
    assert_eq!(initial["models"][0]["supplier"], Value::Null);
    let create = json!({
        "target":{"type":"new","supplier":"office"}, "revision":initial["revision"],
        "preset":"openai", "credential":{"type":"file","path":"private-reference.key"}
    });
    let malformed = response(
        &mut local,
        "modelSuppliers/save",
        json!({"target":"do-not-echo"}),
    );
    assert_eq!(malformed["error"]["code"], -32602);
    assert!(!malformed.to_string().contains("do-not-echo"));
    let saved = rpc(&mut local, "modelSuppliers/save", create.clone());
    assert_ne!(saved["revision"], initial["revision"]);
    assert_eq!(
        response(&mut local, "modelSuppliers/save", create)["error"]["code"],
        -32602
    );
    let binding = json!({
        "target":{"type":"base"}, "revision":saved["revision"], "supplier":"office",
        "protocol":"openai.responses", "model":"office-model", "reasoningEffort":"high"
    });
    let bound = rpc(&mut local, "modelProfiles/bindSupplier", binding);
    let current = rpc(&mut local, "modelSuppliers/list", json!({}));
    assert_eq!(current["revision"], bound["revision"]);
    assert_eq!(
        current["suppliers"][0],
        json!({"id":"office","preset":"openai","baseUrl":"https://api.openai.com/v1/","credential":"configured"})
    );
    assert_eq!(
        current["models"][0],
        json!({"profile":null,"supplier":"office","protocol":"openai.responses","model":"office-model","reasoningEffort":"high"})
    );
    assert!(!current.to_string().contains("private-reference"));
    assert!(!current.to_string().contains("fake-supplier-key"));
    let profiles = rpc(&mut local, "modelProfiles/list", json!({}));
    assert_eq!(profiles["active"]["model"], "offline-original");
    assert_eq!(profiles["profiles"][0]["model"], "office-model");
    let active = rpc(
        &mut local,
        "modelProfiles/select",
        json!({"profile":null,"reload":true}),
    );
    assert_eq!(active["model"], "office-model");
}
