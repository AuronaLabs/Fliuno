//! Real startup, inference budgets, and durable limits on model attempts.
use super::*;
use http_wire::{Fixture, Response};

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "fliuno-model-discovery-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
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
                let messages = request.body["messages"].to_string();
                let over = messages.contains("over budget") || messages.contains("application/pdf");
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
            "windowTokens":32768,"maxInputTokens":30720,"maxInputBytes":null,"maxOutputTokens":2048,
            "countMode":"provider","countCoverage":"provider_request",
            "safetyMarginTokens":0,"source":"provider_metadata"
        })
    );
    let capabilities = &init["capabilities"]["modelCapabilities"];
    assert_eq!(capabilities["provider"], "anthropic");
    assert_eq!(capabilities["model"], "fixture-pinned-20261118");
    assert_eq!(capabilities["modelSource"], "provider_metadata");
    assert_eq!(
        capabilities["supportedInputTypes"],
        json!(["text", "image", "pdf"])
    );
    assert_eq!(capabilities["inputTypesSource"], "provider_adapter");
    assert_eq!(capabilities["budget"], init["capabilities"]["modelBudget"]);
    assert_eq!(capabilities["automaticContextCompaction"], true);
    assert!(capabilities["refreshedAtMs"].as_u64().unwrap() > 0);
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
    let guidance = rejected["output"].as_str().unwrap();
    assert!(guidance.contains("model_budget_rejected"));
    assert!(guidance.contains("schema size of registered tools"));
    assert!(!guidance.contains("image/PDF attachments"));
    assert_eq!(init["capabilities"]["modelBudgetRejectionEvents"], true);
    let events = peer.call("events/list", json!({"taskId":over["id"]}));
    let data = &events["events"]
        .as_array()
        .unwrap()
        .iter()
        .find(|event| event["data"]["type"] == "model_budget_rejected")
        .unwrap()["data"];
    assert_eq!(data["step"], 1);
    assert_eq!(data["summarization"], false);
    let report = &data["report"];
    assert_eq!(report["kind"], "provider_input_tokens");
    assert_eq!(report["limit"], 30720);
    assert_eq!(report["observed"], 30721);
    assert_eq!(report["observedIsLowerBound"], false);
    assert_eq!(report["transportLimitBytes"], 8 * 1024 * 1024);
    assert!(report["serializedGenerationBytes"].as_u64().unwrap() > 0);
    assert_eq!(report["budget"], init["capabilities"]["modelBudget"]);

    let attachment = peer.call(
        "task/start",
        json!({"sessionId":session["id"],"context":{"mode":"isolated"},"input":[
            {"type":"text","text":"Read"},
            {"type":"pdf","data":"JVBERi0xLjQK"}
        ]}),
    );
    let failed = peer.wait_for_status(attachment["id"].as_str().unwrap(), "failed");
    assert_eq!(failed["modelOutcomeUnknown"], false);
    assert!(
        failed["output"]
            .as_str()
            .unwrap()
            .contains("Try smaller or fewer image/PDF attachments")
    );
    let requests = wire.requests();
    assert_eq!(requests.len(), 5);
    assert_eq!(requests[3].path, "/mock/v1/messages/count_tokens");
    assert_eq!(requests[4].path, "/mock/v1/messages/count_tokens");
    assert!(
        requests[4].body["messages"]
            .to_string()
            .contains("application/pdf")
    );
}

#[test]
fn local_budget_rejection_summarizes_history_then_retries_through_scheduler() {
    let wire = Fixture::start(|request| {
        assert_eq!(request.path, "/mock/v1/messages");
        let summary = request.body["messages"]
            .to_string()
            .contains("Summarize the completed work records below");
        let text = if summary {
            "Two earlier tasks were completed.".to_owned()
        } else {
            format!("fact {}", "x".repeat(6000))
        };
        Response::json(
            200,
            json!({"stop_reason":"end_turn","content":[{"type":"text","text":text}]}),
        )
    });
    let directory = TestDirectory::new();
    let child = Command::new(env!("CARGO_BIN_EXE_fliuno-app-server"))
        .arg("--db")
        .arg(directory.db())
        .env("FLIUNO_PROVIDER", "anthropic")
        .env("FLIUNO_MODEL", "fixture-model")
        .env("ANTHROPIC_API_KEY", "test-key")
        .env("ANTHROPIC_BASE_URL", wire.url("/mock/v1"))
        .env("FLIUNO_CONTEXT_WINDOW_TOKENS", "16384")
        .env("FLIUNO_MAX_OUTPUT_TOKENS", "512")
        .env("FLIUNO_CONTEXT_COUNT_MODE", "local")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    let mut peer = Peer::from_child(child);
    peer.call(
        "initialize",
        json!({"protocolVersion":"0.1","clientName":"compaction-process-test"}),
    );
    let workspace = peer.call("workspace/open", json!({"root":directory.0}));
    let session = peer.call("session/create", json!({"workspaceId":workspace["id"]}));
    for input in ["first history record", "second history record"] {
        let started = peer.call(
            "task/start",
            json!({"sessionId":session["id"],"input":input}),
        );
        peer.wait_for_status(started["id"].as_str().unwrap(), "completed");
    }
    let started = peer.call(
        "task/start",
        json!({"sessionId":session["id"],"input":"continue with the earlier work"}),
    );
    let completed = peer.wait_for_status(started["id"].as_str().unwrap(), "completed");
    let stored = peer.call("task/get", json!({"taskId":started["id"]}));
    assert_eq!(completed["output"], format!("fact {}", "x".repeat(6000)));
    assert_eq!(stored["context"]["turns"].as_array().unwrap().len(), 1);
    assert_eq!(stored["context"]["summaries"].as_array().unwrap().len(), 1);
    let events = peer.call("events/list", json!({"taskId":started["id"]}));
    assert!(
        events["events"]
            .as_array()
            .unwrap()
            .iter()
            .any(|event| event["data"]["type"] == "context_compaction_scheduled")
    );
    assert!(
        events["events"]
            .as_array()
            .unwrap()
            .iter()
            .any(|event| event["data"]["type"] == "context_compacted")
    );
    let requests = wire.requests();
    assert_eq!(requests.len(), 4);
    assert!(
        requests[2].body["messages"]
            .to_string()
            .contains("Summarize the completed work records below")
    );
}

#[test]
fn preflight_budget_retry_omits_memory_before_generation_and_keeps_snapshot() {
    const MARKER: &str = "frozen memory body";
    let wire = Fixture::start(|request| match request.path.as_str() {
        "/mock/v1/models/fixture-alias" => Response::json(200, model_metadata(4096)),
        "/mock/v1/messages/count_tokens" => {
            let body = request.body.to_string();
            assert!(body.contains("Explain the sentinel"));
            let tokens = if body.contains(MARKER) { 30721 } else { 30720 };
            Response::json(200, json!({"input_tokens":tokens}))
        }
        "/mock/v1/messages" => {
            let body = request.body.to_string();
            assert!(body.contains("Explain the sentinel"));
            assert!(!body.contains(MARKER));
            Response::json(
                200,
                json!({"stop_reason":"end_turn","content":[{"type":"text","text":"Done"}]}),
            )
        }
        path => panic!("unexpected endpoint {path}"),
    });
    let directory = TestDirectory::new();
    let child = directory
        .command(&wire)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut peer = Peer::from_child(child);
    peer.call(
        "initialize",
        json!({"protocolVersion":"0.1","clientName":"memory-budget-test"}),
    );
    let workspace = peer.call("workspace/open", json!({"root":directory.0}));
    let session = peer.call("session/create", json!({"workspaceId":workspace["id"]}));
    let memory = peer.call(
        "workspace/memory/put",
        json!({"workspaceId":workspace["id"],"key":"sentinel","content":MARKER}),
    );
    let start = json!({"sessionId":session["id"],"input":"Explain the sentinel","maxModelSteps":2,"clientRequestId":"memory-budget"});
    let task = peer.call("task/start", start.clone());
    let id = task["id"].as_str().unwrap();
    peer.wait_for_status(id, "completed");
    let stored = peer.call("task/get", json!({"taskId":id}));
    assert_eq!(stored["modelSteps"], 2);
    assert_eq!(stored["contextMemoryOmittedForBudget"], true);
    assert_eq!(stored["context"]["memories"][0]["id"], memory["id"]);
    assert_eq!(stored["context"]["memories"][0]["content"], MARKER);
    let events = peer.call("events/list", json!({"taskId":id}));
    let report = &events["events"]
        .as_array()
        .unwrap()
        .iter()
        .find(|event| event["data"]["type"] == "model_budget_rejected")
        .unwrap()["data"]["report"];
    let sizes = &report["normalizedInputBytes"];
    assert_eq!(
        sizes["currentText"],
        serde_json::to_vec("Explain the sentinel").unwrap().len()
    );
    assert!(sizes["memory"].as_u64().unwrap() >= MARKER.len() as u64);
    assert!(sizes["toolDefinitions"].as_u64().unwrap() > 0);
    assert_eq!(sizes["currentMedia"], 0);
    assert_eq!(sizes["history"], 0);
    assert_eq!(sizes["toolResults"], 0);
    assert_eq!(sizes["providerReplay"], 0);
    assert!(!report.to_string().contains(MARKER));
    assert!(
        events["events"]
            .as_array()
            .unwrap()
            .iter()
            .any(|event| event["data"]
                == json!({
                    "type":"context_memory_omitted_for_budget","rejectedStep":1,"memoryCount":1,
                }))
    );
    assert_eq!(peer.call("task/start", start)["id"], id);
    let requests = wire.requests();
    assert_eq!(
        requests.iter().map(|r| r.path.as_str()).collect::<Vec<_>>(),
        [
            "/mock/v1/models/fixture-alias",
            "/mock/v1/messages/count_tokens",
            "/mock/v1/messages/count_tokens",
            "/mock/v1/messages",
        ]
    );
    assert!(requests[1].body.to_string().contains(MARKER));
    assert!(!requests[2].body.to_string().contains(MARKER));
}

#[test]
fn budget_retry_reduces_historical_tool_outputs_without_rewriting_live_replay() {
    const OLD_TAIL: &str = "archived-output-tail";
    const LIVE_TAIL: &str = "active-output-tail";
    const FIRST_INPUT: &str = "Read the archived report";
    const NEXT_INPUT: &str = "Read a second report";
    const SIGNATURE: &str = "signed-current-reasoning";

    let wire = Fixture::start(|request| {
        if request.path == "/mock/v1/models/fixture-alias" {
            return Response::json(200, model_metadata(4096));
        }
        let messages = request.body["messages"].as_array().unwrap();
        let later = messages
            .iter()
            .any(|message| message["content"] == NEXT_INPUT);
        let data = request.body.to_string();
        if request.path == "/mock/v1/messages/count_tokens" {
            return Response::json(
                200,
                json!({"input_tokens":if later && data.contains(OLD_TAIL) {30721} else {30720}}),
            );
        }
        assert_eq!(request.path, "/mock/v1/messages");
        let name = if later { "active" } else { "archive" };
        let call_id = format!("tool_{name}");
        if later {
            assert!(!data.contains(OLD_TAIL));
            assert!(data.contains("historical_tool_output_excerpt"));
            let (_, context) = messages[0]["content"]
                .as_str()
                .unwrap()
                .split_once('\n')
                .unwrap();
            let context: Value = serde_json::from_str(context).unwrap();
            let turn = &context["turns"][0];
            let excerpt = &turn["messages"]
                .as_array()
                .unwrap()
                .iter()
                .find(|message| message["role"] == "tool_result")
                .expect("historical tool result")["output"];
            assert_eq!(excerpt["taskId"], turn["taskId"]);
            assert_eq!(excerpt["callId"], "tool_archive");
            assert!(excerpt["originalJsonBytes"].as_u64().unwrap() > 4096);
            let prefix = excerpt["jsonPrefix"].as_str().unwrap();
            assert!(prefix.len() <= 512 && prefix.contains('\u{754c}'));
            assert!(
                messages
                    .iter()
                    .any(|message| message["content"] == NEXT_INPUT)
            );
        }
        if !data.contains(if later { LIVE_TAIL } else { OLD_TAIL }) {
            return Response::json(
                200,
                json!({"stop_reason":"tool_use","content":[
                    {"type":"thinking","thinking":"Preserve this block","signature":SIGNATURE},
                    {"type":"tool_use","id":call_id,"name":"workspace_read_text","input":{"path":format!("{name}.txt")}}
                ]}),
            );
        }
        let result = &messages.last().unwrap()["content"][0];
        assert_eq!(result["type"], "tool_result");
        assert_eq!(result["tool_use_id"], call_id);
        assert!(result["content"].as_str().unwrap().len() > 4096);
        assert_eq!(
            messages[messages.len() - 2]["content"][0]["signature"],
            SIGNATURE
        );
        Response::json(
            200,
            json!({"stop_reason":"end_turn","content":[{"type":"text","text":"Done"}]}),
        )
    });
    let directory = TestDirectory::new();
    for (name, tail) in [("archive", OLD_TAIL), ("active", LIVE_TAIL)] {
        fs::write(
            directory.0.join(format!("{name}.txt")),
            format!("{}{tail}", "\u{754c}".repeat(2500)),
        )
        .unwrap();
    }
    let start = || {
        Peer::from_child(
            directory
                .command(&wire)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .spawn()
                .unwrap(),
        )
    };
    let initialize = |peer: &mut Peer| {
        peer.call(
            "initialize",
            json!({"protocolVersion":"0.1","clientName":"historical-output-budget"}),
        )
    };
    let mut peer = start();
    initialize(&mut peer);
    let workspace = peer.call("workspace/open", json!({"root":directory.0}));
    let session = peer.call("session/create", json!({"workspaceId":workspace["id"]}));
    let complete = |peer: &mut Peer, id: &Value| {
        let id = id.as_str().unwrap();
        let pending = peer.wait_for_status(id, "waiting_approval");
        peer.call("approval/respond", json!({"taskId":id,"approvalId":pending["pendingApproval"]["id"],"decision":"allow_once"}));
        peer.wait_for_status(id, "completed")
    };
    let first = peer.call(
        "task/start",
        json!({"sessionId":session["id"],"input":FIRST_INPUT}),
    );
    let archive = complete(&mut peer, &first["id"]);
    assert!(archive["messages"].to_string().contains(OLD_TAIL));
    let request = json!({"sessionId":session["id"],"input":NEXT_INPUT,"clientRequestId":"live-report","maxModelSteps":3,"context":{"taskIds":[first["id"]]}});
    let next = peer.call("task/start", request.clone());
    let finished = complete(&mut peer, &next["id"]);
    assert_eq!(finished["modelSteps"], 3);
    assert_eq!(finished["context"]["reduceHistoricalToolOutputs"], true);
    assert!(finished["context"]["turns"].to_string().contains(OLD_TAIL));
    assert!(finished["messages"].to_string().contains(LIVE_TAIL));
    let page = peer.call("events/list", json!({"taskId":next["id"]}));
    let types = page["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|event| &event["data"])
        .collect::<Vec<_>>();
    assert_eq!(
        types
            .iter()
            .filter(|event| event["type"] == "context_tool_outputs_reduced")
            .copied()
            .collect::<Vec<_>>(),
        [&json!({"type":"context_tool_outputs_reduced","rejectedStep":1,"outputCount":1})]
    );
    assert_eq!(
        types
            .iter()
            .filter(|event| event["type"] == "tool_started")
            .count(),
        1
    );
    let before_restart = wire.requests();
    assert_eq!(
        before_restart
            .iter()
            .filter(|request| request.path == "/mock/v1/messages/count_tokens")
            .count(),
        5
    );
    assert_eq!(
        before_restart
            .iter()
            .filter(|request| request.path == "/mock/v1/messages")
            .count(),
        4
    );
    drop(peer);
    let mut peer = start();
    initialize(&mut peer);
    assert_eq!(peer.call("task/start", request), finished);
    assert_eq!(
        peer.call("task/get", json!({"taskId":first["id"]})),
        archive
    );
    assert_eq!(wire.requests().len(), before_restart.len() + 1); // startup metadata only
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
fn gemini_provider_count_mode_reports_projected_coverage_and_margin() {
    let wire = Fixture::start(|_| panic!("model preflight unexpectedly contacted the provider"));
    let directory = TestDirectory::new();
    let child = Command::new(env!("CARGO_BIN_EXE_fliuno-app-server"))
        .arg("--db")
        .arg(directory.db())
        .env("FLIUNO_PROVIDER", "gemini")
        .env("FLIUNO_MODEL", "fixture-model")
        .env("GEMINI_API_KEY", "test-gemini-key")
        .env("GEMINI_BASE_URL", wire.url("/v1/"))
        .env("FLIUNO_CONTEXT_WINDOW_TOKENS", "32768")
        .env("FLIUNO_MAX_OUTPUT_TOKENS", "2048")
        .env("FLIUNO_CONTEXT_COUNT_MODE", "provider")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut peer = Peer::from_child(child);
    let init = peer.call(
        "initialize",
        json!({"protocolVersion":"0.1","clientName":"gemini-provider-test"}),
    );
    assert_eq!(
        init["capabilities"]["modelBudget"],
        json!({
            "windowTokens":32768,"maxInputTokens":30720,"maxInputBytes":null,"maxOutputTokens":2048,
            "countMode":"provider","countCoverage":"provider_projection",
            "safetyMarginTokens":256,"source":"configured"
        })
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
        76
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
    assert_eq!(
        peer.call("task/getResumeStatus", lookup.clone()),
        json!({
            "taskId":id,"status":"interrupted","disposition":"model_step_budget_exhausted","remainingModelSteps":0
        })
    );
    assert_eq!(
        peer.request("task/resume", lookup.clone())["error"]["code"],
        -32009
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

#[test]
fn resume_status_requires_consent_only_when_another_model_attempt_is_available() {
    let directory = TestDirectory::new();
    let attempt = std::sync::atomic::AtomicUsize::new(0);
    let wire = Fixture::start(move |_| {
        let first = attempt.fetch_add(1, std::sync::atomic::Ordering::Relaxed) == 0;
        let body = if first {
            json!({"output":[]})
        } else {
            json!({"status":"completed","output":[{"type":"message","content":[{"type":"output_text","text":"recovered"}]}]})
        };
        Response::json(200, body)
    });
    let db = directory.db();
    let url = wire.url("/v1/");
    let mut peer = Peer::start_with_model_fixture(&db, Some(&url));
    let workspace = peer.call("workspace/open", json!({"root":directory.0}));
    let session = peer.call("session/create", json!({"workspaceId":workspace["id"]}));
    let fragment = "retry \"this\" task\n";
    let input = fragment.repeat(250 * 1024 / fragment.len());
    let task = peer.call(
        "task/start",
        json!({"sessionId":session["id"],"input":input,"maxModelSteps":2}),
    );
    let id = task["id"].as_str().unwrap();
    let interrupted = peer.wait_for_status(id, "interrupted");
    let lookup = json!({"taskId":id});
    let before = peer.call("events/list", lookup.clone());
    let status = json!({
        "taskId":id,"status":"interrupted","disposition":"retry_uncertain_model_step","remainingModelSteps":1
    });
    assert_eq!(peer.call("task/getResumeStatus", lookup.clone()), status);
    drop(peer);
    let mut peer = Peer::start_with_model_fixture(&db, Some(&url));
    assert_eq!(peer.call("task/getResumeStatus", lookup.clone()), status);
    assert_eq!(
        peer.request("task/resume", lookup.clone())["error"]["code"],
        -32009
    );
    assert_eq!(peer.call("task/get", lookup.clone()), interrupted);
    assert_eq!(peer.call("events/list", lookup.clone()), before);
    assert_eq!(wire.requests().len(), 1);

    peer.call(
        "task/resume",
        json!({"taskId":id,"retryUncertainModelStep":true}),
    );
    let completed = peer.wait_for_status(id, "completed");
    assert_eq!(completed["output"], "recovered");
    assert_eq!(completed["modelOutcomeUnknown"], false);
    assert_eq!(
        peer.call("task/getResumeStatus", lookup),
        json!({
            "taskId":id,"status":"completed","disposition":"not_interrupted","remainingModelSteps":0
        })
    );
    assert_eq!(wire.requests().len(), 2);
}
