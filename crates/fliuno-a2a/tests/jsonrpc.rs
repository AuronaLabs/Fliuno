//! Real loopback HTTP against the pinned v1.0 JSON-RPC wire contract. These
//! tests are independent fixtures, not official SDK interoperability claims.
#[path = "../../fliuno-mcp/tests/support/http.rs"]
mod http;
use fliuno_a2a::{Client, Error, Options, SendResponse, Task, TaskState};
use http::{Fixture, Request, Response};
use serde_json::{Value, json};
use std::time::Duration;

fn client(fixture: &Fixture) -> Client {
    Client::new(Options {
        endpoint: fixture.url("/rpc"),
        bearer_token: Some("host-secret".into()),
        request_timeout: Duration::from_millis(300),
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
        client(&stalled).send_text("one", "input", None).await,
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
    for mode in 0..4 {
        let peer = Fixture::start(move |request| {
            assert_eq!(request.method, "GET");
            let mut card = json!({"supportedInterfaces":[{"url":format!("http://{}/rpc",request.headers["host"]),"protocolBinding":"JSONRPC","protocolVersion":"1.0"}],"capabilities":{}});
            match mode {
                1 => card["supportedInterfaces"][0]["url"] = json!("https://untrusted.invalid/rpc"),
                2 => card["supportedInterfaces"][0]["protocolVersion"] = json!("0.3"),
                3 => {
                    card["capabilities"]["extensions"] =
                        json!([{"uri":"https://untrusted.invalid/ext","required":true}])
                }
                _ => (),
            }
            Response::json(200, card)
        });
        assert_eq!(
            client(&peer)
                .check_card(&peer.url("/.well-known/agent-card.json"))
                .await
                .is_ok(),
            mode == 0
        );
        assert_eq!(peer.requests().len(), 1);
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
