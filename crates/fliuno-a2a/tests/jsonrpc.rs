//! Real loopback HTTP against the pinned v1.0 JSON-RPC wire contract. These
//! tests are independent fixtures, not official SDK interoperability claims.
#[path = "../../fliuno-mcp/tests/support/http.rs"]
mod http;
use fliuno_a2a::{
    Client, ClientCredentials, Error, Options, SendResponse, StreamEvent, Task, TaskState,
};
use http::{Fixture, Request, Response};
use serde_json::{Value, json};
use std::{
    sync::atomic::{AtomicUsize, Ordering},
    time::Duration,
};

fn client(fixture: &Fixture) -> Client {
    // Allow cold starts and scheduling delays in the emulated Linux runner.
    client_with_timeout(fixture, Duration::from_secs(3))
}
fn client_with_timeout(fixture: &Fixture, request_timeout: Duration) -> Client {
    Client::new(Options {
        endpoint: fixture.url("/rpc"),
        tenant: None,
        optional_extensions: vec![],
        bearer_token: Some("host-secret".into()),
        oauth: None,
        request_timeout,
        max_response_bytes: 1024,
    })
    .unwrap()
}
fn response(request: &Request, result: Value) -> Response {
    Response::json(
        200,
        json!({"jsonrpc":"2.0","id":request.body["id"],"result":result}),
    )
}
fn task(state: &str) -> Value {
    json!({"id":"remote-task","contextId":"context","status":{"state":state}})
}

fn oauth_client(agent: &Fixture, issuer: &Fixture) -> Client {
    Client::new(Options {
        endpoint: agent.url("/rpc"),
        tenant: Some("team/west".into()),
        optional_extensions: vec![],
        bearer_token: None,
        oauth: Some(ClientCredentials {
            token_endpoint: issuer.url("/token"),
            client_id: "test-application".into(),
            client_secret: "literal+fixture&secret".into(),
            scope: Some("api://fliuno/.default".into()),
        }),
        request_timeout: Duration::from_secs(3),
        max_response_bytes: 4096,
    })
    .unwrap()
}

#[tokio::test]
async fn tenant_and_oauth_bind_every_remote_operation_and_expiring_tokens_refresh() {
    let issued = AtomicUsize::new(0);
    let issuer = Fixture::start(move |request| {
        assert_eq!(request.path, "/token");
        assert!(!request.headers.contains_key("authorization"));
        assert_eq!(
            request.body.as_str().unwrap(),
            "grant_type=client_credentials&client_id=test-application&client_secret=literal%2Bfixture%26secret&scope=api%3A%2F%2Ffliuno%2F.default"
        );
        let n = issued.fetch_add(1, Ordering::SeqCst);
        Response::json(
            200,
            json!({"access_token":format!("fixture-token-{n}"),"token_type":"Bearer","expires_in":if n == 0 { 1 } else { 300 }}),
        )
    });
    let agent = Fixture::start(|request| {
        assert_eq!(request.headers["a2a-version"], "1.0");
        if request.method == "GET" {
            return Response::json(
                200,
                json!({"supportedInterfaces":[{"url":format!("http://{}/rpc",request.headers["host"]),"protocolBinding":"JSONRPC","protocolVersion":"1.0","tenant": if request.path == "/card" { "team/west" } else { "other" }}],"capabilities":{"streaming":true}}),
            );
        }
        assert_eq!(request.body["params"]["tenant"], "team/west");
        assert!(!request.body.to_string().contains("fixture-token"));
        match request.body["method"].as_str().unwrap() {
            "SendMessage" => response(request, json!({"task":task("TASK_STATE_SUBMITTED")})),
            "GetTask" => response(request, task("TASK_STATE_WORKING")),
            "CancelTask" => response(request, task("TASK_STATE_CANCELED")),
            "SubscribeToTask" => {
                stream_response(request, [json!({"task":task("TASK_STATE_COMPLETED")})])
            }
            "SendStreamingMessage" => {
                stream_response(request, [json!({"task":task("TASK_STATE_COMPLETED")})])
            }
            _ => panic!("unexpected method"),
        }
    });
    let client = oauth_client(&agent, &issuer);
    client.authenticate().await.unwrap();
    client
        .check_card_streaming(&agent.url("/card"))
        .await
        .unwrap();
    assert!(matches!(
        client.check_card(&agent.url("/wrong-card")).await,
        Err(Error::Protocol)
    ));
    // Once expired, each path must attach the replacement token without replaying a request.
    tokio::time::sleep(Duration::from_secs(1)).await;
    client.send_text("attempt", "text", None).await.unwrap();
    client.get_task("remote-task").await.unwrap();
    client.cancel_task("remote-task").await.unwrap();
    let mut events = client
        .send_text_streaming("attempt-stream", "text", None)
        .await
        .unwrap();
    assert!(matches!(
        events.recv().await.unwrap().unwrap(),
        StreamEvent::Task(_)
    ));
    let mut events = client
        .subscribe_to_task("remote-task", "context")
        .await
        .unwrap();
    assert!(matches!(
        events.recv().await.unwrap().unwrap(),
        StreamEvent::Task(_)
    ));
    let requests = agent.requests();
    assert_eq!(requests.len(), 7);
    assert_eq!(
        requests[0].headers["authorization"],
        "Bearer fixture-token-0"
    );
    assert!(
        requests[2..]
            .iter()
            .all(|r| r.headers["authorization"] == "Bearer fixture-token-1")
    );
    assert_eq!(issuer.requests().len(), 2);
}

#[tokio::test]
async fn oauth_failure_stays_at_the_issuer_and_unauthorized_submission_is_not_replayed() {
    let agent = Fixture::start(|_| Response::json(401, json!({"error":"sensitive-response"})));
    let rejected =
        Fixture::start(|_| Response::json(400, json!({"error_description":"sensitive-response"})));
    let client = oauth_client(&agent, &rejected);
    let result = client.send_text("attempt", "text", None).await;
    assert!(matches!(result, Err(Error::Authentication)));
    assert!(
        !result
            .err()
            .unwrap()
            .to_string()
            .contains("sensitive-response")
    );
    assert!(agent.requests().is_empty());
    let authorized = Fixture::start(|_| {
        Response::json(
            200,
            json!({"access_token":"fixture-token","token_type":"Bearer","expires_in":300}),
        )
    });
    assert!(matches!(
        oauth_client(&agent, &authorized)
            .send_text("attempt", "text", None)
            .await,
        Err(Error::Http(401))
    ));
    assert_eq!(authorized.requests().len(), 1);
    assert_eq!(agent.requests().len(), 1);
}

#[tokio::test]
async fn v1_methods_envelope_headers_and_task_fields_match_stable_contract() {
    let peer = Fixture::start(|request| {
        assert_eq!(request.path, "/rpc");
        assert_eq!(request.headers["a2a-version"], "1.0");
        assert_eq!(request.headers["authorization"], "Bearer host-secret");
        assert_eq!(request.headers["content-type"], "application/json");
        assert_eq!(request.headers["accept"], "application/json");
        match request.body["method"].as_str().unwrap() {
            "SendMessage" => {
                assert_eq!(
                    request.body["params"]["message"],
                    json!({"messageId":"attempt","role":"ROLE_USER","parts":[{"text":"history"},{"text":"do work"}]})
                );
                assert_eq!(
                    request.body["params"]["configuration"]["returnImmediately"],
                    true
                );
                response(request, json!({"task":task("TASK_STATE_SUBMITTED")}))
            }
            "GetTask" => {
                assert_eq!(
                    request.body["params"],
                    json!({"id":"remote-task","historyLength":0})
                );
                response(request, task("TASK_STATE_WORKING"))
            }
            "CancelTask" => {
                assert_eq!(request.body["params"], json!({"id":"remote-task"}));
                response(request, task("TASK_STATE_CANCELED"))
            }
            method => panic!("unexpected method {method}"),
        }
    });
    let client = client(&peer);
    let SendResponse::Task(first) = client
        .send_text("attempt", "do work", Some("history"))
        .await
        .unwrap()
    else {
        panic!("expected task")
    };
    assert_eq!(first.state, TaskState::Submitted);
    assert!(client.get_task(&first.id).await.unwrap().matches(&first));
    assert!(
        client
            .cancel_task(&first.id)
            .await
            .unwrap()
            .state
            .is_terminal()
    );
    assert_eq!(peer.requests().len(), 3);
}

#[tokio::test]
async fn streaming_submission_decodes_status_and_artifact_events_with_backpressure_contract() {
    let peer = Fixture::start(|request| {
        assert_eq!(request.body["method"], "SendStreamingMessage");
        assert_eq!(request.headers["accept"], "text/event-stream");
        assert_eq!(request.headers["a2a-version"], "1.0");
        assert_eq!(request.headers["authorization"], "Bearer host-secret");
        assert_eq!(
            request.body["params"]["message"]["parts"],
            json!([{"text":"history"},{"text":"do work"}])
        );
        let id = request.body["id"].as_str().unwrap();
        let events = [
            json!({"task":task("TASK_STATE_WORKING")}),
            json!({"statusUpdate":{"taskId":"remote-task","contextId":"context","status":{"state":"TASK_STATE_WORKING"}}}),
            json!({"artifactUpdate":{"taskId":"remote-task","contextId":"context","artifact":{"artifactId":"report","name":"Report","parts":[{"text":"ready"}]},"append":false,"lastChunk":true}}),
            json!({"statusUpdate":{"taskId":"remote-task","contextId":"context","status":{"state":"TASK_STATE_COMPLETED","message":{"messageId":"final","taskId":"remote-task","contextId":"context","role":"ROLE_AGENT","parts":[{"text":"done"}]}}}}),
        ];
        let body: String = events
            .into_iter()
            .map(|result| {
                format!(
                    "data: {}\r\n\r\n",
                    json!({"jsonrpc":"2.0","id":id,"result":result})
                )
            })
            .collect();
        Response::sse(format!(": heartbeat\r\n\r\n{body}"))
    });
    let client = client(&peer);
    let mut events = client
        .send_text_streaming("attempt", "do work", Some("history"))
        .await
        .unwrap();
    assert_eq!(events.max_capacity(), 16);
    let mut received = Vec::new();
    while let Some(event) = events.recv().await {
        received.push(event.unwrap());
    }
    assert_eq!(received.len(), 4);
    assert!(matches!(received[0], StreamEvent::Task(ref task) if task.id == "remote-task"));
    assert!(
        matches!(received[1], StreamEvent::StatusUpdate { ref task_id, state: TaskState::Working, .. } if task_id == "remote-task")
    );
    assert!(
        matches!(received[2], StreamEvent::ArtifactUpdate { ref artifact, last_chunk: Some(true), .. } if artifact.id() == "report")
    );
    assert!(matches!(
        received[3],
        StreamEvent::StatusUpdate {
            state: TaskState::Completed,
            ..
        }
    ));
    assert_eq!(peer.requests().len(), 1);
}

#[tokio::test]
async fn streaming_response_rejects_a_changed_remote_task_identity() {
    let peer = Fixture::start(|request| {
        let id = request.body["id"].as_str().unwrap();
        let frames = [
            json!({"task":task("TASK_STATE_WORKING")}),
            json!({"artifactUpdate":{"taskId":"other-task","contextId":"context","artifact":{"artifactId":"x","parts":[{"text":"x"}]}}}),
        ];
        Response::sse(
            frames
                .into_iter()
                .map(|result| {
                    format!(
                        "data: {}\n\n",
                        json!({"jsonrpc":"2.0","id":id,"result":result})
                    )
                })
                .collect(),
        )
    });
    let mut events = client(&peer)
        .send_text_streaming("attempt", "input", None)
        .await
        .unwrap();
    assert!(matches!(
        events.recv().await.unwrap().unwrap(),
        StreamEvent::Task(_)
    ));
    assert!(matches!(events.recv().await.unwrap(), Err(Error::Protocol)));
}

#[tokio::test]
async fn streaming_response_enforces_the_total_byte_budget() {
    let peer = Fixture::start(|_| Response::sse(format!(": {}\n\n", "x".repeat(1100))));
    assert!(matches!(
        client(&peer)
            .send_text_streaming("attempt", "input", None)
            .await,
        Err(Error::Budget)
    ));
}

fn stream_response(request: &Request, frames: impl IntoIterator<Item = Value>) -> Response {
    Response::sse(
        frames
            .into_iter()
            .map(|result| {
                format!(
                    "data: {}\n\n",
                    json!({"jsonrpc":"2.0","id":request.body["id"],"result":result})
                )
            })
            .collect(),
    )
}

fn status_update(state: &str, context: &str) -> Value {
    json!({"statusUpdate":{"taskId":"remote-task","contextId":context,"status":{"state":state}}})
}

#[tokio::test]
async fn subscription_observes_existing_task_across_pause_and_completion() {
    let peer = Fixture::start(|request| {
        assert_eq!(request.body["method"], "SubscribeToTask");
        assert_eq!(request.body["params"], json!({"id":"remote-task"}));
        assert_eq!(request.headers["accept"], "text/event-stream");
        stream_response(
            request,
            [
                json!({"task":task("TASK_STATE_INPUT_REQUIRED")}),
                status_update("TASK_STATE_WORKING", "context"),
                status_update("TASK_STATE_COMPLETED", "context"),
            ],
        )
    });
    let mut stream = client(&peer)
        .subscribe_to_task("remote-task", "context")
        .await
        .unwrap();
    assert!(matches!(
        stream.recv().await,
        Some(Ok(StreamEvent::Task(Task {
            state: TaskState::InputRequired,
            ..
        })))
    ));
    for expected in [TaskState::Working, TaskState::Completed] {
        assert!(
            matches!(stream.recv().await, Some(Ok(StreamEvent::StatusUpdate { state, .. })) if state == expected)
        );
    }
    assert!(stream.recv().await.is_none());
    assert_eq!(peer.requests().len(), 1);
}

#[tokio::test]
async fn subscription_requires_snapshot_and_does_not_accept_a_foreign_context_or_pause_as_eof() {
    for mode in 0..4 {
        let peer = Fixture::start(move |request| {
            let frames = match mode {
                0 => vec![status_update("TASK_STATE_WORKING", "context")],
                1 => vec![
                    json!({"task":{"id":"remote-task","contextId":"foreign","status":{"state":"TASK_STATE_WORKING"}}}),
                ],
                2 => vec![
                    json!({"task":task("TASK_STATE_WORKING")}),
                    status_update("TASK_STATE_COMPLETED", "foreign"),
                ],
                _ => vec![json!({"task":task("TASK_STATE_INPUT_REQUIRED")})],
            };
            stream_response(request, frames)
        });
        let mut stream = client(&peer)
            .subscribe_to_task("remote-task", "context")
            .await
            .unwrap();
        if mode >= 2 {
            assert!(matches!(
                stream.recv().await,
                Some(Ok(StreamEvent::Task(_)))
            ));
        }
        assert!(matches!(stream.recv().await, Some(Err(Error::Protocol))));
        assert!(stream.recv().await.is_none());
        assert_eq!(peer.requests().len(), 1);
    }
}

#[tokio::test]
async fn subscription_preserves_a_json_rpc_rejection_without_retry() {
    let peer = Fixture::start(|request| {
        Response::json(
            200,
            json!({
                "jsonrpc":"2.0","id":request.body["id"],"error":{"code":-32004,"message":"private detail"}
            }),
        )
    });
    let client = client(&peer);
    assert!(matches!(
        client.subscribe_to_task("", "context").await,
        Err(Error::Config)
    ));
    assert!(matches!(
        client.subscribe_to_task("remote-task", "context").await,
        Err(Error::Remote(-32004))
    ));
    assert_eq!(peer.requests().len(), 1);
}

#[tokio::test]
async fn immediate_message_preserves_text_data_bytes_urls_and_metadata() {
    let message = json!({"messageId":"reply","role":"ROLE_AGENT","parts":[{"text":"hello"},{"data":{"count":3}},{"raw":"aGk=","mediaType":"text/plain","filename":"hi.txt"},{"url":"https://elsewhere.invalid/file","metadata":{"inert":true}}],"metadata":{"vendor":"retained"}});
    let expected = message.clone();
    let peer = Fixture::start(move |request| response(request, json!({"message":message})));
    let SendResponse::Message(reply) = client(&peer).send_text("one", "input", None).await.unwrap()
    else {
        panic!("expected message")
    };
    assert_eq!(reply.raw, expected);
    assert_eq!(reply.text(), "hello");
}

#[test]
fn rejects_obsolete_or_ambiguous_content_and_inconsistent_identity() {
    for invalid in [
        json!({"id":"t","status":{"state":"completed"}}),
        json!({"id":"t","status":{"state":"TASK_STATE_UNKNOWN"}}),
        json!({"id":"t","status":{"state":"TASK_STATE_COMPLETED"},"artifacts":[{"artifactId":"a","parts":[{"kind":"text","text":"old"}]}]}),
        json!({"id":"t","status":{"state":"TASK_STATE_COMPLETED"},"artifacts":[{"artifactId":"a","parts":[{"text":"x","data":{}}]}]}),
        json!({"id":"t","status":{"state":"TASK_STATE_COMPLETED"},"artifacts":[{"artifactId":"a","parts":[{"raw":"not base64!!"}]}]}),
        json!({"id":"t","contextId":"c","status":{"state":"TASK_STATE_COMPLETED","message":{"messageId":"m","role":"ROLE_AGENT","taskId":"another","parts":[{"text":"wrong task"}]}}}),
        json!({"id":"t","status":{"state":"TASK_STATE_COMPLETED"},"artifacts":[{"artifactId":"same","parts":[{"text":"a"}]},{"artifactId":"same","parts":[{"text":"b"}]}]}),
    ] {
        assert!(Task::parse(invalid.clone()).is_err(), "accepted {invalid}");
    }
    let first = Task::parse(task("TASK_STATE_SUBMITTED")).unwrap();
    let mut changed = task("TASK_STATE_COMPLETED");
    changed["contextId"] = json!("other");
    assert!(!Task::parse(changed).unwrap().matches(&first));
}

#[tokio::test]
async fn correlation_and_response_union_are_checked_without_retry() {
    for mode in 0..5 {
        let peer = Fixture::start(move |request| {
            let message = json!({"messageId":"m","role":"ROLE_AGENT","parts":[{"text":"ok"}]});
            let mut value =
                json!({"jsonrpc":"2.0","id":request.body["id"],"result":{"message":message}});
            match mode {
                0 => value["id"] = json!("other"),
                1 => value["jsonrpc"] = json!("1.0"),
                2 => value["error"] = json!({"code":-32600,"message":"secret"}),
                3 => value["result"]["task"] = task("TASK_STATE_COMPLETED"),
                _ => value["result"]["message"]["role"] = json!("ROLE_USER"),
            }
            Response::json(200, value)
        });
        assert!(matches!(
            client(&peer).send_text("one", "input", None).await,
            Err(Error::Protocol)
        ));
        assert_eq!(peer.requests().len(), 1);
    }
}

#[tokio::test]
async fn remote_error_is_numeric_and_never_exposes_peer_diagnostics() {
    let peer = Fixture::start(|request| {
        Response::json(
            200,
            json!({"jsonrpc":"2.0","id":request.body["id"],"error":{"code":-32006,"message":"peer-secret","data":{"credential":"host-secret"}}}),
        )
    });
    let error = client(&peer)
        .send_text("one", "input", None)
        .await
        .unwrap_err();
    assert!(matches!(error, Error::Remote(-32006)));
    assert_eq!(error.to_string(), "A2A remote error -32006");
}

#[tokio::test]
async fn response_body_and_timeout_are_bounded_and_one_shot() {
    let huge = Fixture::start(|request| {
        response(
            request,
            json!({"message":{"messageId":"m","role":"ROLE_AGENT","parts":[{"text":"x".repeat(2048)}]}}),
        )
    });
    assert!(matches!(
        client(&huge).send_text("one", "input", None).await,
        Err(Error::Budget)
    ));
    let stalled = Fixture::start(|_| Response::hold());
    assert!(matches!(
        client_with_timeout(&stalled, Duration::from_millis(300))
            .send_text("one", "input", None)
            .await,
        Err(Error::Transport)
    ));
    assert_eq!(stalled.requests().len(), 1);
}

#[tokio::test]
async fn redirect_does_not_forward_credentials_or_submit_to_a_second_endpoint() {
    let destination = Fixture::start(|_| panic!("redirect must not be followed"));
    let target = destination.url("/capture");
    let peer =
        Fixture::start(move |_| Response::json(307, Value::Null).header("Location", &target));
    assert!(matches!(
        client(&peer).send_text("one", "input", None).await,
        Err(Error::Http(307))
    ));
    assert!(destination.requests().is_empty());
}

#[tokio::test]
async fn pinned_card_validates_version_interface_and_required_extensions() {
    for mode in 0..5 {
        let peer = Fixture::start(move |request| {
            assert_eq!(request.method, "GET");
            let mut card = json!({"supportedInterfaces":[{"url":format!("http://{}/rpc",request.headers["host"]),"protocolBinding":"JSONRPC","protocolVersion":"1.0"}],"capabilities":{"streaming":true}});
            match mode {
                1 => card["supportedInterfaces"][0]["url"] = json!("https://untrusted.invalid/rpc"),
                2 => card["supportedInterfaces"][0]["protocolVersion"] = json!("0.3"),
                3 => {
                    card["capabilities"]["extensions"] =
                        json!([{"uri":"https://untrusted.invalid/ext","required":true}])
                }
                4 => card["capabilities"]["streaming"] = json!(false),
                _ => (),
            }
            Response::json(200, card)
        });
        assert_eq!(
            client(&peer)
                .check_card(&peer.url("/.well-known/agent-card.json"))
                .await
                .is_ok(),
            mode == 0 || mode == 4
        );
        assert_eq!(
            client(&peer)
                .check_card_streaming(&peer.url("/.well-known/agent-card.json"))
                .await
                .is_ok(),
            mode == 0
        );
        assert_eq!(peer.requests().len(), 2);
    }
}

#[test]
fn endpoint_policy_requires_https_or_literal_loopback_without_embedded_credentials() {
    for value in [
        "http://localhost/rpc",
        "http://192.0.2.1/rpc",
        "file:///tmp/x",
        "https://user:pass@example.com/rpc",
        "https://example.com/rpc#x",
        "https://example.com/rpc?key=secret",
    ] {
        assert!(fliuno_a2a::validate_endpoint(value).is_err());
    }
    for value in [
        "https://example.com/rpc",
        "http://127.0.0.1:1234/rpc",
        "http://[::1]:1234/rpc",
    ] {
        assert!(fliuno_a2a::validate_endpoint(value).is_ok());
    }
}

#[test]
fn protojson_binary_accepts_both_base64_alphabets_and_optional_padding() {
    for raw in ["aGk=", "aGk", "+/8=", "-_8=", "-_8"] {
        let mut value = task("TASK_STATE_COMPLETED");
        value["artifacts"] = json!([{"artifactId":"binary","parts":[{"raw":raw,"mediaType":"application/octet-stream"}]}]);
        assert!(
            Task::parse(value).is_ok(),
            "rejected valid ProtoJSON bytes {raw}"
        );
    }
}
