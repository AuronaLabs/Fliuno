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

#[test]
fn supplier_catalog_uses_only_the_saved_route_and_rejects_stale_queries() {
    use std::{
        io::{BufRead, BufReader, Write},
        net::TcpListener,
        time::{Duration, Instant},
    };

    fn serve(listener: &TcpListener, target: &str, auth: Option<&str>, status: u16, body: &str) {
        let deadline = Instant::now() + Duration::from_secs(8);
        let mut socket = loop {
            match listener.accept() {
                Ok((socket, _)) => break socket,
                Err(error)
                    if error.kind() == std::io::ErrorKind::WouldBlock
                        && Instant::now() < deadline =>
                {
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(error) => panic!("no catalog request: {error}"),
            }
        };
        socket
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut reader = BufReader::new(&mut socket);
        let mut request = String::new();
        loop {
            let offset = request.len();
            assert!(reader.read_line(&mut request).unwrap() > 0);
            if &request[offset..] == "\r\n" {
                break;
            }
        }
        assert!(
            request.starts_with(&format!("GET {target} HTTP/1.1\r\n")),
            "{request}"
        );
        let headers = request.to_ascii_lowercase();
        assert!(!headers.contains("\nauthorization:"));
        if let Some(auth) = auth {
            assert!(headers.contains(&format!("\nx-api-key: {auth}\r\n")));
        } else {
            assert!(!headers.contains("\nx-api-key:"));
        }
        write!(socket, "HTTP/1.1 {status} Fixture\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
    }

    let root = ConfigFixture(
        std::env::temp_dir().join(format!("fliuno-catalog-{}", uuid::Uuid::new_v4())),
    );
    fs::create_dir(&root.0).unwrap();
    let config = root.0.join("config.toml");
    let lab = TcpListener::bind("127.0.0.1:0").unwrap();
    let local = TcpListener::bind("127.0.0.1:0").unwrap();
    lab.set_nonblocking(true).unwrap();
    local.set_nonblocking(true).unwrap();
    let text = format!(
        r#"
[providers.lab]
base_url = "http://{}/lab/v1/"
api_key_file = "catalog.key"
[providers.local]
base_url = "http://{}/local/v1/"
auth = "none"
"#,
        lab.local_addr().unwrap(),
        local.local_addr().unwrap()
    );
    fs::write(&config, &text).unwrap();
    fs::write(root.0.join("catalog.key"), "catalog-fixture-key").unwrap();
    let mut host = RpcServer::new(Kernel::in_memory(Box::new(fliuno_core::DemoProvider)).unwrap());
    host.enable_local_model_profiles(config.clone(), None);
    let mut attached = host.attach_connection().unwrap();
    for (server, enabled) in [(&mut host, true), (&mut attached, false)] {
        assert_eq!(
            rpc(
                server,
                "initialize",
                json!({"protocolVersion":"0.1","clientName":"catalog-test"})
            )["capabilities"]["modelSupplierCatalog"],
            enabled
        );
    }
    let original = rpc(&mut host, "modelSuppliers/list", json!({}));
    let query = |supplier: &str, protocol: &str, cursor: Option<&str>| {
        json!({
            "supplier": supplier, "protocol": protocol, "revision": original["revision"], "cursor": cursor,
        })
    };
    let lab_query = query("lab", "claude.message", None);
    let method = "modelSuppliers/listModels";
    assert_eq!(
        response(&mut attached, method, lab_query.clone())["error"]["code"],
        -32601
    );
    assert_eq!(
        response(&mut host, method, query("local", "claude.message", None))["error"]["code"],
        -32602
    );
    assert_eq!(
        response(&mut host, method, query("other", "claude.message", None))["error"]["code"],
        -32602
    );
    let mut spoofed = lab_query.clone();
    spoofed["baseUrl"] = json!("http://wrong.test");
    assert_eq!(
        response(&mut host, method, spoofed)["error"]["code"],
        -32602
    );
    assert_eq!(
        lab.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
    assert_eq!(
        local.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );

    std::thread::scope(|scope| {
        scope.spawn(|| {
            serve(&lab, "/lab/v1/models?limit=100", Some("catalog-fixture-key"), 200,
                r#"{"data":[{"id":"claude-first","display_name":"echo catalog-fixture-key"},{"id":"catalog-fixture-key"}],"has_more":true,"last_id":"claude-first"}"#);
            serve(&lab, "/lab/v1/models?limit=100&after_id=claude-first", Some("catalog-fixture-key"), 200,
                r#"{"data":[{"id":"claude-next","display_name":"Second page model"}],"has_more":false}"#);
        });
        scope.spawn(|| {
            serve(
                &local,
                "/local/v1/models",
                None,
                404,
                r#"{"error":"unsupported"}"#,
            )
        });
        let first = rpc(&mut host, method, lab_query.clone());
        assert_eq!(first["page"]["models"], json!([{"id":"claude-first"}]));
        assert_eq!(first["page"]["nextCursor"], "claude-first");
        assert!(!first.to_string().contains("catalog-fixture-key"));
        let fallback = rpc(&mut host, method, query("local", "openai.compatible", None));
        assert_eq!(fallback["catalog"], "unavailable");
        assert_eq!(fallback["httpStatus"], 404);
        let next = rpc(
            &mut host,
            method,
            query(
                "lab",
                "claude.message",
                first["page"]["nextCursor"].as_str(),
            ),
        );
        assert_eq!(
            next["page"]["models"],
            json!([{"id":"claude-next","displayName":"Second page model"}])
        );
        assert!(next["page"]["nextCursor"].is_null());
    });
    assert_eq!(fs::read_to_string(&config).unwrap(), text);
    let amended = format!("# external edit\n{text}");
    fs::write(&config, &amended).unwrap();
    assert_eq!(
        response(&mut host, method, lab_query)["error"]["code"],
        -32009
    );
    assert_eq!(
        lab.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
    assert_eq!(fs::read_to_string(&config).unwrap(), amended);
}
