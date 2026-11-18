use fliuno_agents::{A2aConfig, register_a2a_config};
use fliuno_core::{DemoProvider, Kernel};
use serde_json::{Value, json};
#[path = "../../fliuno-mcp/tests/support/http.rs"]
mod http;
use fliuno_core::{AgentObservation, AgentObserver, CancellationToken, PreparedAgentRun};
use fliuno_protocol::{AgentStopReason, TaskBackend};
use std::sync::atomic::{AtomicUsize, Ordering};

fn register(kernel: &mut Kernel, value: Value) -> Result<(), String> {
    register_a2a_config(kernel, serde_json::from_value::<A2aConfig>(value).unwrap())
}
#[test]
fn registration_is_atomic_and_shares_the_agent_id_budget() {
    let mut kernel = Kernel::in_memory(Box::new(DemoProvider)).unwrap();
    assert!(register(&mut kernel, json!({"agents":[{"id":"valid","endpoint":"https://example.invalid/rpc"},{"id":"bad","endpoint":"http://example.invalid/rpc"}]})).is_err());
    assert!(kernel.agent_specs().is_empty());
    let config = json!({"agents":[{"id":"remote","endpoint":"https://example.invalid/rpc"}]});
    register(&mut kernel, config.clone()).unwrap();
    assert!(register(&mut kernel, config).is_err());
    assert_eq!(kernel.agent_specs().len(), 1);
}
#[test]
fn cards_credentials_timeouts_and_unknown_options_fail_before_registration() {
    for field in [
        json!({"agentCardUrl":"https://other.invalid/card"}),
        json!({"bearerTokenEnv":"invalid-name"}),
        json!({"bearerTokenEnv":"FLIUNO_A2A_TEST_NONEXISTENT_SECRET_37C4"}),
        json!({"requestTimeoutMs":0}),
        json!({"taskTimeoutMs":0}),
        json!({"pollIntervalMs":0}),
        json!({"maxResponseBytes":16}),
    ] {
        let mut kernel = Kernel::in_memory(Box::new(DemoProvider)).unwrap();
        let mut agent = json!({"id":"remote","endpoint":"https://example.invalid/rpc"});
        agent
            .as_object_mut()
            .unwrap()
            .extend(field.as_object().unwrap().clone());
        assert!(register(&mut kernel, json!({"agents":[agent]})).is_err());
        assert!(kernel.agent_specs().is_empty());
    }
    assert!(serde_json::from_value::<A2aConfig>(json!({"agents":[{"id":"remote","endpoint":"https://example.invalid/rpc","workspaceAccess":true}]})).is_err());
}

struct Observer {
    reject: bool,
    cancellation: Option<CancellationToken>,
    records: AtomicUsize,
}
impl AgentObserver for Observer {
    fn observe(&self, _: AgentObservation) -> fliuno_core::Result<()> {
        if self.reject {
            return Err(fliuno_core::Error::Conflict(
                "injected failed commit".into(),
            ));
        }
        if self.records.fetch_add(1, Ordering::SeqCst) == 0
            && let Some(token) = &self.cancellation
        {
            token.cancel();
        }
        Ok(())
    }
}
fn prepared(endpoint: String, token: &CancellationToken) -> PreparedAgentRun {
    prepared_with_streaming(endpoint, token, false)
}
fn prepared_with_streaming(
    endpoint: String,
    token: &CancellationToken,
    streaming: bool,
) -> PreparedAgentRun {
    let mut kernel = Kernel::in_memory(Box::new(DemoProvider)).unwrap();
    register(
        &mut kernel,
        json!({"agents":[{"id":"remote","endpoint":endpoint,"streaming":streaming}]}),
    )
    .unwrap();
    let ws = kernel
        .open_workspace(std::env::current_dir().unwrap().to_str().unwrap())
        .unwrap();
    let session = kernel.create_session(&ws.id).unwrap();
    let (task, _) = kernel
        .create_task_with_backend(
            &session.id,
            "test",
            None,
            TaskBackend::ExternalAgent {
                agent_id: "remote".into(),
            },
        )
        .unwrap();
    kernel.prepare_agent_run(&task.id, token).unwrap().unwrap()
}
#[test]
fn failed_identity_commit_prevents_all_followup_network_operations() {
    let peer = http::Fixture::start(|request| {
        assert_eq!(request.body["method"], "SendMessage");
        http::Response::json(
            200,
            json!({"jsonrpc":"2.0","id":request.body["id"],"result":{"task":{"id":"t","contextId":"c","status":{"state":"TASK_STATE_WORKING"}}}}),
        )
    });
    let token = CancellationToken::new();
    let observer = Observer {
        reject: true,
        cancellation: None,
        records: AtomicUsize::new(0),
    };
    let outcome = prepared(peer.url("/rpc"), &token).execute(&observer, &token);
    assert!(outcome.outcome_unknown);
    assert_eq!(peer.requests().len(), 1);
}
#[test]
fn cancellation_racing_first_identity_observation_still_cancels_that_remote_task() {
    let peer = http::Fixture::start(|request| {
        let result = match request.body["method"].as_str().unwrap() {
            "SendMessage" => {
                json!({"task":{"id":"t","contextId":"c","status":{"state":"TASK_STATE_WORKING"}}})
            }
            "CancelTask" => {
                json!({"id":"t","contextId":"c","status":{"state":"TASK_STATE_CANCELED"}})
            }
            other => panic!("unexpected operation after initial cancellation: {other}"),
        };
        http::Response::json(
            200,
            json!({"jsonrpc":"2.0","id":request.body["id"],"result":result}),
        )
    });
    let token = CancellationToken::new();
    let observer = Observer {
        reject: false,
        cancellation: Some(token.clone()),
        records: AtomicUsize::new(0),
    };
    let outcome = prepared(peer.url("/rpc"), &token).execute(&observer, &token);
    assert!(!outcome.outcome_unknown);
    assert_eq!(outcome.stop_reason, Some(AgentStopReason::Cancelled));
    assert_eq!(observer.records.load(Ordering::SeqCst), 2);
    assert_eq!(peer.requests().len(), 2);
}

#[test]
fn streaming_cancellation_uses_the_persisted_remote_identity_once() {
    let peer = http::Fixture::start(|request| {
        let id = request.body["id"].as_str().unwrap();
        let result = match request.body["method"].as_str().unwrap() {
            "SendStreamingMessage" => {
                let frame = json!({"jsonrpc":"2.0","id":id,"result":{"task":{"id":"t","contextId":"c","status":{"state":"TASK_STATE_WORKING"}}}});
                return http::Response::sse(format!("data: {frame}\n\n"));
            }
            "CancelTask" => {
                json!({"id":"t","contextId":"c","status":{"state":"TASK_STATE_CANCELED"}})
            }
            other => panic!("unexpected operation {other}"),
        };
        http::Response::json(200, json!({"jsonrpc":"2.0","id":id,"result":result}))
    });
    let token = CancellationToken::new();
    let observer = Observer {
        reject: false,
        cancellation: Some(token.clone()),
        records: AtomicUsize::new(0),
    };
    let outcome =
        prepared_with_streaming(peer.url("/rpc"), &token, true).execute(&observer, &token);
    assert!(!outcome.outcome_unknown);
    assert_eq!(outcome.stop_reason, Some(AgentStopReason::Cancelled));
    let requests = peer.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].body["method"], "SendStreamingMessage");
    assert_eq!(requests[1].body["method"], "CancelTask");
}
