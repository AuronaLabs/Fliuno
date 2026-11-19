use super::*;
use http_wire::{Fixture, Response};

#[test]
fn openai_usage_is_replayable_per_step_even_when_the_tool_call_is_rejected() {
    let root = std::env::temp_dir()
        .canonicalize()
        .unwrap()
        .join(format!("fliuno-model-usage-{}", uuid::Uuid::new_v4()));
    fs::create_dir(&root).unwrap();
    let endpoint = Fixture::start(|request| {
        let input = request.body["input"].as_array().unwrap();
        let reject = input.iter().any(|item| item["content"] == "reject");
        let invalid_numbers = input
            .iter()
            .any(|item| item["content"] == "invalid numbers");
        let output = if reject {
            json!([{"type":"function_call","call_id":"unknown","name":"not_registered","arguments":"{}"}])
        } else {
            json!([{"type":"message","content":[{"type":"output_text","text":"Finished"}]}])
        };
        let mut usage = json!({
            "input_tokens":120,"output_tokens":24,
            "input_tokens_details":{"cached_tokens":80},
            "output_tokens_details":{"reasoning_tokens":8}
        });
        if invalid_numbers {
            usage["output_tokens"] = json!(-1);
        }
        Response::json(
            200,
            json!({"status":"completed","output":output,"usage":usage}),
        )
    });
    let db = root.join("kernel.db");
    let mut peer = Peer::start_with_model_fixture(&db, Some(&endpoint.url("/v1/")));
    let workspace = peer.call("workspace/open", json!({"root":root}));
    let session = peer.call("session/create", json!({"workspaceId":workspace["id"]}));
    let mut tasks = Vec::new();
    for (input, status, expected_event) in [
        ("finish", "completed", "task_completed"),
        ("reject", "failed", "task_failed"),
        ("invalid numbers", "completed", "task_completed"),
    ] {
        let task = peer.call(
            "task/start",
            json!({"sessionId":session["id"],"input":input}),
        );
        let id = task["id"].as_str().unwrap().to_owned();
        peer.wait_for_status(&id, status);
        tasks.push((id, expected_event));
    }
    drop(peer);
    // A different provider on restart can still replay the original source.
    let mut peer = Peer::start(&db);
    for (index, (id, expected_event)) in tasks.into_iter().enumerate() {
        let page = peer.call("events/list", json!({"taskId":id}));
        let events = page["events"].as_array().unwrap();
        assert_eq!(events.last().unwrap()["data"]["type"], expected_event);
        let usage: Vec<_> = events
            .iter()
            .filter(|event| event["data"]["type"] == "model_usage_reported")
            .collect();
        if index == 2 {
            assert!(usage.is_empty());
        } else {
            assert_eq!(usage.len(), 1);
            assert_eq!(
                usage[0]["data"],
                json!({
                    "type":"model_usage_reported","step":1,"usage":{
                        "provider":"openai","inputTokens":120,"outputTokens":24,
                        "cachedInputTokens":80,"reasoningOutputTokens":8
                    }
                })
            );
            assert_eq!(events[events.len() - 2], *usage[0]);
        }
    }
    assert_eq!(endpoint.requests().len(), 3);
    drop(peer);
    fs::remove_dir_all(root).unwrap();
}
