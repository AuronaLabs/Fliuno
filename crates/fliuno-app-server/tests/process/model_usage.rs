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
        .execute_batch("DROP TABLE model_usage; PRAGMA user_version=36;")
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

#[test]
fn scoped_model_usage_pages_keep_order_and_watermark_after_v39_upgrade() {
    let root = std::env::temp_dir().join(format!("fliuno-usage-scope-{}", uuid::Uuid::new_v4()));
    fs::create_dir_all(root.join("other")).unwrap();
    let endpoint = Fixture::start(|_| {
        Response::json(
            200,
            json!({
                "status":"completed",
                "output":[{"type":"message","content":[{"type":"output_text","text":"Done"}]}],
                "usage":{"input_tokens":21,"output_tokens":5}
            }),
        )
    });
    let db = root.join("kernel.db");
    let mut peer = Peer::start_with_model_fixture(&db, Some(&endpoint.url("/v1/")));
    let workspace = peer.call("workspace/open", json!({"root":root}));
    let foreign = peer.call("workspace/open", json!({"root":root.join("other")}));
    let first = peer.call("session/create", json!({"workspaceId":workspace["id"]}));
    let second = peer.call("session/create", json!({"workspaceId":workspace["id"]}));
    let isolated = peer.call("session/create", json!({"workspaceId":foreign["id"]}));
    let submit = |peer: &mut Peer, session: &Value| {
        let task = peer.call(
            "task/start",
            json!({"sessionId":session["id"],"input":"usage report"}),
        );
        peer.wait_for_status(task["id"].as_str().unwrap(), "completed");
        let mut page = peer.call("events/listModelUsage", json!({"taskId":task["id"]}));
        let reports = page["events"].as_array_mut().unwrap();
        assert_eq!(reports.len(), 1);
        reports.pop().unwrap()
    };
    let a = submit(&mut peer, &first);
    let b = submit(&mut peer, &isolated);
    let c = submit(&mut peer, &second);
    let through = peer.call("events/getWatermark", json!({}))["throughSequence"].clone();
    let method = "events/listWorkspaceModelUsage";
    let mut query =
        json!({"workspaceId":workspace["id"],"through":through,"limit":1,"targetBytes":1024});
    let page = peer.call(method, query.clone());
    assert_eq!(
        page,
        json!({"events":[a],"hasMore":true,"nextAfter":a["sequence"]})
    );
    let d = submit(&mut peer, &first);
    assert_eq!(peer.call(method, query.clone()), page);
    peer.call(
        "session/setArchived",
        json!({"sessionId":second["id"],"archived":true}),
    );
    drop(peer);

    // Recreate the v37-v39 index layout, leaving the durable events unchanged.
    rusqlite::Connection::open(&db)
        .unwrap()
        .execute_batch(
            "BEGIN;
             CREATE TABLE model_usage_index(
                task_id TEXT NOT NULL REFERENCES tasks(id),
                sequence INTEGER NOT NULL REFERENCES events(sequence),
                PRIMARY KEY(task_id,sequence)
             ) WITHOUT ROWID;
             INSERT INTO model_usage_index SELECT task_id,sequence FROM model_usage;
             DROP TABLE model_usage;
             PRAGMA user_version=39;
             COMMIT;",
        )
        .unwrap();
    let mut peer = Peer::start(&db);
    query["after"] = a["sequence"].clone();
    assert_eq!(
        peer.call(method, query),
        json!({"events":[c],"hasMore":false,"nextAfter":c["sequence"]})
    );
    assert_eq!(
        peer.call(method, json!({"workspaceId":workspace["id"]}))["events"],
        json!([a, c, d])
    );
    assert_eq!(
        peer.call(method, json!({"workspaceId":foreign["id"]}))["events"],
        json!([b])
    );
    let session_method = "events/listSessionModelUsage";
    assert_eq!(
        peer.call(
            session_method,
            json!({"sessionId":first["id"],"through":through})
        )["events"],
        json!([a])
    );
    assert_eq!(
        peer.call(session_method, json!({"sessionId":first["id"]}))["events"],
        json!([a, d])
    );
    assert_eq!(
        peer.call(session_method, json!({"sessionId":second["id"]}))["events"],
        json!([c])
    );
    assert_eq!(
        peer.request(session_method, json!({"sessionId":"missing"}))["error"]["code"],
        -32004
    );
    assert_eq!(
        peer.request(method, json!({"workspaceId":"missing"}))["error"]["code"],
        -32004
    );
    assert_eq!(
        peer.request(
            method,
            json!({"workspaceId":workspace["id"],"through":u64::MAX})
        )["error"]["code"],
        -32602
    );
    assert_eq!(
        peer.request(
            session_method,
            json!({"sessionId":first["id"],"targetBytes":1023})
        )["error"]["code"],
        -32602
    );
    drop(peer);
    let db_conn = rusqlite::Connection::open(&db).unwrap();
    assert_eq!(
        db_conn
            .pragma_query_value(None, "user_version", |row| row.get::<_, u32>(0))
            .unwrap(),
        41
    );
    assert_eq!(
        db_conn
            .query_row("SELECT COUNT(*) FROM model_usage", [], |row| row
                .get::<_, i64>(0))
            .unwrap(),
        4
    );
    assert_eq!(
        db_conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_schema WHERE name='model_usage_index'",
                [],
                |row| row.get::<_, i64>(0)
            )
            .unwrap(),
        0
    );
    drop(db_conn);
    fs::remove_dir_all(root).unwrap();
}
