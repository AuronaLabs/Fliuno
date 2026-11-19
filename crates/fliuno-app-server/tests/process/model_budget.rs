//! Real startup, inference budgets, and durable limits on model attempts.
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

#[test]
fn gemini_interactions_is_wired_through_app_server_without_remote_storage() {
    let wire = Fixture::start(|request| {
        assert_eq!(request.method, "POST");
        assert_eq!(request.path, "/v1/interactions");
        assert_eq!(request.headers["x-goog-api-key"], "test-gemini-key");
        assert_eq!(request.headers["api-revision"], "2026-05-20");
        assert_eq!(request.body["store"], false);
        Response::json(
            200,
            json!({"status":"completed","steps":[{
                "type":"model_output","content":[{"type":"text","text":"Gemini completed"}]
            }]}),
        )
    });
    let directory = TestDirectory::new();
    let mut command = Command::new(env!("CARGO_BIN_EXE_fliuno-app-server"));
    command
        .arg("--db")
        .arg(directory.db())
        .env("FLIUNO_PROVIDER", "gemini")
        .env("FLIUNO_MODEL", "fixture-model")
        .env("GEMINI_API_KEY", "test-gemini-key")
        .env("GEMINI_BASE_URL", wire.url("/v1/"))
        .env_remove("OPENAI_API_KEY")
        .env_remove("ANTHROPIC_API_KEY")
        .env_remove("FLIUNO_CONTEXT_WINDOW_TOKENS")
        .env_remove("FLIUNO_MAX_OUTPUT_TOKENS")
        .env_remove("FLIUNO_CONTEXT_COUNT_MODE")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    let mut peer = Peer::from_child(command.spawn().unwrap());
    peer.call(
        "initialize",
        json!({"protocolVersion":"0.1","clientName":"gemini-provider-test"}),
    );
    let workspace = peer.call("workspace/open", json!({"root":directory.0}));
    let session = peer.call("session/create", json!({"workspaceId":workspace["id"]}));
    let accepted = peer.call(
        "task/start",
        json!({"sessionId":session["id"],"input":"Say hello"}),
    );
    let done = peer.wait_for_status(accepted["id"].as_str().unwrap(), "completed");
    assert_eq!(done["output"], "Gemini completed");
    assert_eq!(wire.requests().len(), 1);
}

#[test]
fn gemini_provider_count_mode_fails_before_database_creation() {
    let wire = Fixture::start(|_| panic!("unsupported provider counting sent a request"));
    let directory = TestDirectory::new();
    let result = Command::new(env!("CARGO_BIN_EXE_fliuno-app-server"))
        .arg("--db")
        .arg(directory.db())
        .env("FLIUNO_PROVIDER", "gemini")
        .env("FLIUNO_MODEL", "fixture-model")
        .env("GEMINI_API_KEY", "test-gemini-key")
        .env("GEMINI_BASE_URL", wire.url("/v1/"))
        .env("FLIUNO_CONTEXT_WINDOW_TOKENS", "32768")
        .env("FLIUNO_MAX_OUTPUT_TOKENS", "2048")
        .env("FLIUNO_CONTEXT_COUNT_MODE", "provider")
        .output()
        .unwrap();
    assert!(!result.status.success());
    assert!(!directory.db().exists());
    assert!(
        String::from_utf8(result.stderr)
            .unwrap()
            .contains("preflight provider token counting")
    );
    assert!(wire.requests().is_empty());
}

#[test]
fn model_step_limit_is_durable_across_receipt_retry_and_final_approval() {
    let directory = TestDirectory::new();
    let db_path = directory.db();
    let mut peer = Peer::start(&db_path);
    let workspace = peer.call("workspace/open", json!({"root":directory.0}));
    let session = peer.call("session/create", json!({"workspaceId":workspace["id"]}));
    let request = json!({"sessionId":session["id"],"input":"approval at the limit",
        "clientRequestId":"one-step","maxModelSteps":1});
    let started = peer.call("task/startSummary", request.clone());
    let id = started["id"].as_str().unwrap();
    let waiting = peer.wait_for_status(id, "waiting_approval");
    assert_eq!(waiting["maxModelSteps"], 1);
    assert_eq!(waiting["modelSteps"], 1);
    drop(peer);

    let mut peer = Peer::start(&db_path);
    assert_eq!(peer.call("task/start", request.clone()), waiting);
    assert_eq!(peer.call("task/startSummary", request.clone())["id"], id);
    let events = |peer: &mut Peer| peer.call("events/list", json!({"taskId":id}));
    let before = events(&mut peer);
    let mut changed = request;
    changed["maxModelSteps"] = json!(2);
    for method in ["task/startSummary", "task/start"] {
        assert_eq!(
            peer.request(method, changed.clone())["error"]["code"],
            -32009
        );
    }
    assert_eq!(events(&mut peer), before);

    peer.call(
        "approval/respond",
        json!({"taskId":id,
        "approvalId":waiting["pendingApproval"]["id"],"decision":"allow_once"}),
    );
    let done = peer.wait_for_status(id, "failed");
    assert_eq!(done["output"], "model step budget exceeded");
    assert_eq!(done["modelSteps"], 1);
    assert_eq!(done["modelOutcomeUnknown"], false);
    assert_eq!(
        peer.call("artifacts/list", json!({"taskId":id}))
            .as_array()
            .unwrap()
            .len(),
        1
    );
    let page = events(&mut peer);
    let kinds: Vec<_> = page["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|event| event["data"]["type"].as_str().unwrap())
        .collect();
    assert_eq!(
        kinds
            .iter()
            .filter(|kind| **kind == "model_started")
            .count(),
        1
    );
    assert!(
        kinds.ends_with(&["artifact_created", "tool_completed", "task_failed"]),
        "{kinds:?}"
    );
}

#[test]
fn model_step_limit_preserves_default_receipts_and_rejects_invalid_settings() {
    let directory = TestDirectory::new();
    let db_path = directory.db();
    let mut peer = Peer::start(&db_path);
    let workspace = peer.call("workspace/open", json!({"root":directory.0}));
    let session = peer.call("session/create", json!({"workspaceId":workspace["id"]}));
    let request =
        json!({"sessionId":session["id"],"input":"default steps","clientRequestId":"old"});
    let started = peer.call("task/start", request.clone());
    let id = started["id"].as_str().unwrap();
    let waiting = peer.wait_for_status(id, "waiting_approval");
    assert!(waiting.get("maxModelSteps").is_none());
    drop(peer);

    let db = rusqlite::Connection::open(&db_path).unwrap();
    db.pragma_update(None, "user_version", 45).unwrap();
    let old_options = || {
        db.query_row::<String, _, _>(
            "SELECT options FROM task_start_receipts WHERE request_key='old'",
            [],
            |row| row.get(0),
        )
        .unwrap()
    };
    let v45 = old_options();
    let options: Value = serde_json::from_str(&v45).unwrap();
    assert_eq!(options.as_array().unwrap().len(), 2);
    let mut peer = Peer::start(&db_path);
    assert_eq!(
        db.pragma_query_value::<u32, _>(None, "user_version", |r| r.get(0))
            .unwrap(),
        46
    );
    let mut default = request.clone();
    default["maxModelSteps"] = json!(8);
    assert_eq!(peer.call("task/startSummary", default.clone())["id"], id);
    assert_eq!(peer.call("task/start", default), waiting);
    assert_eq!(old_options(), v45);

    for invalid in [
        json!({"maxModelSteps":0}),
        json!({"maxModelSteps":9}),
        json!({"maxModelSteps":1,"backend":{"type":"external_agent","agentId":"missing"}}),
    ] {
        let mut params = request.clone();
        params["clientRequestId"] = json!("invalid");
        params
            .as_object_mut()
            .unwrap()
            .extend(invalid.as_object().unwrap().clone());
        assert_eq!(
            peer.request("task/startSummary", params)["error"]["code"],
            -32602
        );
    }
    assert_eq!(
        db.query_row::<u32, _, _>("SELECT count(*) FROM tasks", [], |row| row.get(0))
            .unwrap(),
        1
    );
}

#[test]
fn model_step_limit_keeps_uncertain_outcome_when_last_attempt_cannot_resume() {
    let directory = TestDirectory::new();
    // The request reached the loopback service, but it did not return a usable
    // model response. This is intentionally classified as outcome unknown.
    let endpoint = Fixture::start(|_| Response::json(200, json!({"output":[]})));
    let db = directory.db();
    let url = endpoint.url("/v1/");
    let mut peer = Peer::start_with_model_fixture(&db, Some(&url));
    let workspace = peer.call("workspace/open", json!({"root":directory.0}));
    let session = peer.call("session/create", json!({"workspaceId":workspace["id"]}));
    let task = peer.call(
        "task/start",
        json!({"sessionId":session["id"],"input":"one attempt","maxModelSteps":1}),
    );
    let id = task["id"].as_str().unwrap();
    let interrupted = peer.wait_for_status(id, "interrupted");
    assert_eq!(interrupted["modelOutcomeUnknown"], true);
    assert_eq!(interrupted["modelSteps"], 1);
    drop(peer);

    let mut peer = Peer::start_with_model_fixture(&db, Some(&url));
    let lookup = json!({"taskId":id});
    let page = peer.call("events/list", lookup.clone());
    assert!(
        page["events"]
            .as_array()
            .unwrap()
            .iter()
            .any(|event| event["data"]["type"] == "model_outcome_unknown"
                && event["data"]["step"] == 1)
    );
    let response = peer.request(
        "task/resume",
        json!({"taskId":id,"retryUncertainModelStep":true}),
    );
    assert_eq!(response["error"]["code"], -32009);
    assert!(
        response["error"]["message"]
            .as_str()
            .unwrap()
            .contains("budget exhausted")
    );
    assert_eq!(peer.call("task/get", lookup.clone()), interrupted);
    assert_eq!(peer.call("events/list", lookup), page);
    assert_eq!(endpoint.requests().len(), 1);
}
