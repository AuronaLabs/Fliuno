use super::*;
use std::sync::{Mutex, mpsc::SyncSender, mpsc::sync_channel};

fn legacy_config(fixture: &Fixture) -> HttpServerConfig {
    serde_json::from_value(json!({
        "id":"remote", "transport":"http_sse", "url":fixture.url("/events/sse"), "timeoutMs":2000
    }))
    .unwrap()
}

#[test]
fn legacy_http_sse_uses_a_persistent_get_and_never_replays_an_unknown_post() {
    for disconnect_on_call in [false, true] {
        let events = Mutex::new(None::<SyncSender<String>>);
        let fixture = Fixture::start(move |request| {
            if request.method == "GET" {
                assert_eq!(request.path, "/events/sse");
                assert_eq!(request.headers["accept"], "text/event-stream");
                let (send, recv) = sync_channel(8);
                *events.lock().unwrap() = Some(send);
                return Response::live_sse(
                    ": hello\r\n\r\nevent: endpoint\r\ndata: ../messages?session=loopback\r\n\r\n"
                        .into(),
                    recv,
                );
            }
            assert_eq!(request.method, "POST");
            assert_eq!(request.path, "/messages?session=loopback");
            let method = request.body["method"].as_str().unwrap();
            let mut events = events.lock().unwrap();
            let result = match method {
                "initialize" => {
                    assert_eq!(request.body["params"]["protocolVersion"], "2024-11-05");
                    assert_eq!(request.body["params"]["capabilities"], json!({}));
                    json!({"protocolVersion":"2024-11-05", "capabilities":{"tools":{}},
                        "serverInfo":{"name":"legacy-fixture","version":"1"}})
                }
                "notifications/initialized" => return Response::json(202, Value::Null),
                "tools/list" => json!({"tools":[{"name":"echo","inputSchema":{"type":"object"}}]}),
                "tools/call" if disconnect_on_call => {
                    events.take();
                    return Response::json(202, Value::Null);
                }
                "tools/call" => json!({"content":[{"type":"text","text":"legacy result"}]}),
                _ => panic!("unexpected method: {method}"),
            };
            events
                .as_ref()
                .unwrap()
                .send(format!(
                    "event: message\r\ndata: {}\r\n\r\n",
                    json!({
                        "jsonrpc":"2.0","id":request.body["id"],"result":result
                    })
                ))
                .unwrap();
            Response::json(202, Value::Null)
        });
        let registered = tools(legacy_config(&fixture));
        let cancellation = CancellationToken::new();
        let result = invoke(&*registered[0], &cancellation);
        if disconnect_on_call {
            assert!(matches!(result, Err(ToolError::OutcomeUnknown(_))));
            assert!(
                matches!(invoke(&*registered[0], &cancellation), Err(ToolError::Failed(message)) if message.contains("host is closed"))
            );
        } else {
            assert_eq!(
                result.unwrap().output["content"][0]["text"],
                "legacy result"
            );
        }
        let requests = fixture.requests();
        assert_eq!(requests.iter().filter(|r| r.method == "GET").count(), 1);
        assert_eq!(
            requests
                .iter()
                .filter(|r| r.body["method"] == "tools/call")
                .count(),
            1
        );
        assert!(
            !requests
                .iter()
                .any(|r| r.body["method"] == "server/discover")
        );
    }
}

#[test]
fn legacy_http_sse_rejects_cross_origin_or_oversized_endpoint_events() {
    let foreign = Fixture::start(|_| Response::text(200, "must not be reached"));
    for endpoint in [foreign.url("/post"), "/".repeat(1024 * 1024)] {
        let fixture = Fixture::start(move |request| {
            assert_eq!(request.method, "GET");
            Response::sse(format!("event: endpoint\ndata: {endpoint}\n\n"))
        });
        assert!(crate::connect_server(ServerConfig::Http(legacy_config(&fixture))).is_err());
        assert_eq!(fixture.requests().len(), 1);
        assert!(foreign.requests().is_empty());
    }
}
