//! Real startup, handshake and inference using the configured metadata peer.
use super::*;
use http_wire::{Fixture, Response};

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "fliuno-model-discovery-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }

    fn db(&self) -> PathBuf {
        self.0.join("kernel.db")
    }

    fn command(&self, endpoint: &Fixture) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_fliuno-app-server"));
        command
            .arg("--db")
            .arg(self.db())
            .env("FLIUNO_PROVIDER", "anthropic")
            .env("FLIUNO_MODEL", "fixture-alias")
            .env("FLIUNO_CONTEXT_WINDOW_TOKENS", "provider")
            .env("FLIUNO_MAX_OUTPUT_TOKENS", "2048")
            .env("FLIUNO_CONTEXT_COUNT_MODE", "provider")
            .env("ANTHROPIC_BASE_URL", endpoint.url("/mock/v1"))
            .env("ANTHROPIC_API_KEY", "test-metadata-key");
        command
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        if let Ok(entries) = fs::read_dir(&self.0) {
            for entry in entries.flatten() {
                let _ = fs::remove_file(entry.path());
            }
        }
        let _ = fs::remove_dir(&self.0);
    }
}

fn model_metadata(max_output: u32) -> Value {
    json!({"type":"model","id":"fixture-pinned-20261118","lifecycle":"active",
        "max_input_tokens":32768,"max_tokens":max_output})
}

#[test]
fn model_discovery_pins_alias_for_count_and_generation_and_reports_the_budget() {
    let wire = Fixture::start(|request| {
        assert_eq!(request.headers["x-api-key"], "test-metadata-key");
        assert_eq!(request.headers["anthropic-version"], "2023-06-01");
        match request.path.as_str() {
            "/mock/v1/models/fixture-alias" => {
                assert_eq!(request.method, "GET");
                assert!(request.body.is_null());
                Response::json(200, model_metadata(4096))
            }
            "/mock/v1/messages/count_tokens" => {
                assert_eq!(request.method, "POST");
                assert_eq!(request.body["model"], "fixture-pinned-20261118");
                assert!(request.body.get("max_tokens").is_none());
                let over = request.body["messages"].to_string().contains("over budget");
                Response::json(200, json!({"input_tokens":if over {30721} else {30720}}))
            }
            "/mock/v1/messages" => {
                assert_eq!(request.method, "POST");
                assert_eq!(request.body["model"], "fixture-pinned-20261118");
                assert_eq!(request.body["max_tokens"], 2048);
                assert!(!request.body["messages"].to_string().contains("over budget"));
                Response::json(
                    200,
                    json!({"stop_reason":"end_turn","content":[{"type":"text","text":"Done"}]}),
                )
            }
            path => panic!("unexpected model endpoint {path}"),
        }
    });
    let directory = TestDirectory::new();
    let child = directory
        .command(&wire)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut peer = Peer::from_child(child);
    let init = peer.call(
        "initialize",
        json!({"protocolVersion":"0.1","clientName":"discovery-test"}),
    );
    assert_eq!(
        init["capabilities"]["modelBudget"],
        json!({
            "windowTokens":32768,"maxInputTokens":30720,"maxOutputTokens":2048,
            "countMode":"provider","source":"provider_metadata"
        })
    );
    assert_eq!(wire.requests().len(), 1);
    let workspace = peer.call("workspace/open", json!({"root":directory.0}));
    let session = peer.call("session/create", json!({"workspaceId":workspace["id"]}));
    let accepted = peer.call(
        "task/start",
        json!({"sessionId":session["id"],"input":"fits"}),
    );
    let done = peer.wait_for_status(accepted["id"].as_str().unwrap(), "completed");
    assert_eq!(done["output"], "Done");
    let over = peer.call(
        "task/start",
        json!({"sessionId":session["id"],"input":"over budget"}),
    );
    let rejected = peer.wait_for_status(over["id"].as_str().unwrap(), "failed");
    assert_eq!(rejected["modelOutcomeUnknown"], false);
    let requests = wire.requests();
    assert_eq!(requests.len(), 4);
    assert_eq!(requests[3].path, "/mock/v1/messages/count_tokens");
}

#[test]
fn unusable_model_metadata_stops_startup_before_database_creation() {
    let private = "fixture-secret-in-body";
    for (status, value, diagnostic) in [
        (
            401,
            json!({"message":private}),
            "metadata response was unsuccessful or invalid",
        ),
        (
            200,
            json!({"type":"model","id":"fixture","max_tokens":4096}),
            "did not include valid limits",
        ),
        (200, model_metadata(1024), "output limit exceeds"),
        (
            200,
            json!({"type":"model","id":"fixture","max_input_tokens":32768,"max_tokens":4096,"data":"x".repeat(65536)}),
            "metadata response was unsuccessful or invalid",
        ),
    ] {
        let wire = Fixture::start(move |request| {
            assert_eq!(request.path, "/mock/v1/models/fixture-alias");
            Response::json(status, value.clone())
        });
        let directory = TestDirectory::new();
        let result = directory.command(&wire).output().unwrap();
        assert!(!result.status.success());
        assert!(!directory.db().exists());
        assert!(result.stdout.is_empty());
        assert_eq!(wire.requests().len(), 1);
        let stderr = String::from_utf8(result.stderr).unwrap();
        assert!(stderr.contains(diagnostic), "{stderr}");
        assert!(!stderr.contains(private));
        assert!(!stderr.contains("test-metadata-key"));
    }
}

#[test]
fn openai_requires_numeric_window_without_using_a_metadata_endpoint() {
    let wire = Fixture::start(|_| panic!("unsupported discovery must not send a request"));
    let directory = TestDirectory::new();
    let result = directory
        .command(&wire)
        .env("FLIUNO_PROVIDER", "openai")
        .env("OPENAI_API_KEY", "fixture")
        .env("OPENAI_BASE_URL", wire.url("/v1/"))
        .output()
        .unwrap();
    assert!(!result.status.success());
    assert!(!directory.db().exists());
    let error = String::from_utf8(result.stderr).unwrap();
    assert!(error.contains("unsupported for OpenAI"), "{error}");
    assert!(wire.requests().is_empty());
}
