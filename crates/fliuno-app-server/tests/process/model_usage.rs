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
        let first_of_two = input.iter().any(|item| item["content"] == "two steps")
            && !input
                .iter()
                .any(|item| item["type"] == "function_call_output");
        let output = if first_of_two {
            json!([{"type":"function_call","call_id":"artifact","name":"artifact_create",
                "arguments":json!({"title":"result","content":"body"}).to_string()}])
        } else if reject {
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
    let multi = peer.call(
        "task/start",
        json!({"sessionId":session["id"],"input":"two steps"}),
    );
    let multi_id = multi["id"].as_str().unwrap();
    let waiting = peer.wait_for_status(multi_id, "waiting_approval");
    let first = peer.call(
        "events/listModelUsage",
        json!({"taskId":multi_id,"limit":1}),
    );
    assert_eq!(first["events"][0]["data"]["step"], 1);
    assert_eq!(first["hasMore"], false);
    peer.call(
        "approval/respond",
        json!({
            "taskId":multi_id,"approvalId":waiting["pendingApproval"]["id"],"decision":"allow_once"
        }),
    );
    peer.wait_for_status(multi_id, "completed");
    let current = peer.call(
        "events/listModelUsage",
        json!({"taskId":multi_id,"limit":1}),
    );
    assert_eq!(current["events"], first["events"]);
    assert_eq!(current["nextAfter"], first["nextAfter"]);
    assert_eq!(current["hasMore"], true);
    let second_query = json!({"taskId":multi_id,"after":first["nextAfter"],"limit":1});
    let second = peer.call("events/listModelUsage", second_query.clone());
    assert_eq!(second["events"][0]["data"]["step"], 2);
    assert_eq!(second["hasMore"], false);
    assert_eq!(
        peer.call(
            "events/listModelUsage",
            json!({
                "taskId":multi_id,"through":first["nextAfter"]
            })
        ),
        first
    );
    drop(peer);
    rusqlite::Connection::open(&db)
        .unwrap()
        .execute_batch("DROP TABLE model_usage_index; PRAGMA user_version=36;")
        .unwrap();
    // A different provider on restart can still replay the original source.
    let mut peer = Peer::start(&db);
    assert_eq!(peer.call("events/listModelUsage", second_query), second);
    for (index, (id, expected_event)) in tasks.into_iter().enumerate() {
        let page = peer.call("events/list", json!({"taskId":id}));
        let events = page["events"].as_array().unwrap();
        assert_eq!(events.last().unwrap()["data"]["type"], expected_event);
        let usage: Vec<_> = events
            .iter()
            .filter(|event| event["data"]["type"] == "model_usage_reported")
            .collect();
        assert_eq!(
            peer.call("events/listModelUsage", json!({"taskId":id}))["events"],
            json!(usage)
        );
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
    assert_eq!(endpoint.requests().len(), 5);
    drop(peer);
    fs::remove_dir_all(root).unwrap();
}
