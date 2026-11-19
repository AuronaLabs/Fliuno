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
    // This probe verifies the scoped usage migration and its indexed rows.
    // Later unrelated migrations can raise the version further.
    assert!(
        db_conn
            .pragma_query_value(None, "user_version", |row| row.get::<_, u32>(0))
            .unwrap()
            >= 40
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

#[test]
fn usage_summary_is_exact_and_scoped_after_v42_migration_and_new_reports() {
    let root = std::env::temp_dir().join(format!("fliuno-usage-totals-{}", uuid::Uuid::new_v4()));
    fs::create_dir_all(root.join("foreign")).unwrap();
    let endpoint = Fixture::start(|request| {
        let last = request.body["input"].as_array().unwrap().last().unwrap();
        let (input, output, cached, reasoning) = match last["content"].as_str().unwrap() {
            "first" => (u64::MAX, 6, Some(0), None),
            "archived" => (u64::MAX, 0, None, Some(0)),
            "isolated" => (12, 1, Some(10), None),
            "after upgrade" => (5, 8, Some(4), Some(3)),
            other => panic!("unexpected fixture input: {other}"),
        };
        Response::json(
            200,
            json!({
                "status":"completed",
                "output":[{"type":"message","content":[{"type":"output_text","text":"Done"}]}],
                "usage":{"input_tokens":input,"output_tokens":output,
                    "input_tokens_details":{"cached_tokens":cached},
                    "output_tokens_details":{"reasoning_tokens":reasoning}}
            }),
        )
    });
    let db = root.join("kernel.db");
    let url = endpoint.url("/v1/");
    let mut peer = Peer::start_with_model_fixture(&db, Some(&url));
    let workspace = peer.call("workspace/open", json!({"root":root}));
    let other_workspace = peer.call("workspace/open", json!({"root":root.join("foreign")}));
    let first = peer.call("session/create", json!({"workspaceId":workspace["id"]}));
    let archived = peer.call("session/create", json!({"workspaceId":workspace["id"]}));
    let empty = peer.call("session/create", json!({"workspaceId":workspace["id"]}));
    let isolated = peer.call(
        "session/create",
        json!({"workspaceId":other_workspace["id"]}),
    );
    let submit = |peer: &mut Peer, session: &Value, input| {
        let task = peer.call(
            "task/startSummary",
            json!({
                "sessionId":session["id"],"input":input,"context":{"mode":"isolated"}
            }),
        );
        peer.wait_for_status(task["id"].as_str().unwrap(), "completed");
        task
    };
    let summary = |peer: &mut Peer, scope: &Value| {
        let summary = peer.call("modelUsage/getSummary", json!({"scope":scope}));
        assert_eq!(
            summary["throughSequence"],
            peer.call("events/getWatermark", json!({}))["throughSequence"]
        );
        summary
    };
    let totals = |reports: u64,
                  input: u128,
                  output: u64,
                  cached: (u64, u64),
                  reasoning: (u64, u64)| {
        json!({"reportCount":reports.to_string(),"inputTokens":input.to_string(),"outputTokens":output.to_string(),
            "cachedInputTokens":cached.0.to_string(),"cachedInputReportCount":cached.1.to_string(),
            "reasoningOutputTokens":reasoning.0.to_string(),"reasoningOutputReportCount":reasoning.1.to_string()})
    };
    let scope = json!({"kind":"workspace","workspaceId":workspace["id"]});
    let first_scope = json!({"kind":"session","sessionId":first["id"]});
    let empty_scope = json!({"kind":"session","sessionId":empty["id"]});
    assert_eq!(
        summary(&mut peer, &empty_scope)["totals"],
        totals(0, 0, 0, (0, 0), (0, 0))
    );
    let task = submit(&mut peer, &first, "first");
    let isolated_summary = summary(&mut peer, &scope);
    submit(&mut peer, &isolated, "isolated");
    let next = summary(&mut peer, &scope);
    assert_eq!(next["totals"], isolated_summary["totals"]);
    assert!(
        next["throughSequence"].as_u64().unwrap()
            > isolated_summary["throughSequence"].as_u64().unwrap()
    );
    let first_use = totals(1, u64::MAX.into(), 6, (0, 1), (0, 0));
    assert_eq!(
        summary(&mut peer, &json!({"kind":"task","taskId":task["id"]}))["totals"],
        first_use
    );
    assert_eq!(summary(&mut peer, &first_scope)["totals"], first_use);
    submit(&mut peer, &archived, "archived");
    peer.call(
        "session/setArchived",
        json!({"sessionId":archived["id"],"archived":true}),
    );
    let before = summary(&mut peer, &scope);
    let doubled = u128::from(u64::MAX) * 2;
    assert_eq!(before["totals"], totals(2, doubled, 6, (0, 1), (0, 1)));
    let before_cursor = before["throughSequence"].as_u64().unwrap();
    drop(peer);

    // Remove only the new projection to reproduce an actual v42 database.
    rusqlite::Connection::open(&db)
        .unwrap()
        .execute_batch("DROP TABLE model_usage_totals; PRAGMA user_version=42;")
        .unwrap();
    let mut peer = Peer::start_with_model_fixture(&db, Some(&url));
    assert_eq!(summary(&mut peer, &scope), before);
    assert_eq!(summary(&mut peer, &first_scope)["totals"], first_use);
    let task_after = submit(&mut peer, &first, "after upgrade");
    let after = summary(&mut peer, &scope);
    assert_eq!(after["totals"], totals(3, doubled + 5, 14, (4, 2), (3, 2)));
    assert_eq!(
        summary(&mut peer, &first_scope)["totals"],
        totals(2, u128::from(u64::MAX) + 5, 14, (4, 2), (3, 1))
    );
    let suffix = peer.call(
        "events/listWorkspaceModelUsage",
        json!({"workspaceId":workspace["id"],"after":before_cursor}),
    );
    assert_eq!(suffix["events"].as_array().unwrap().len(), 1);
    assert_eq!(suffix["events"][0]["taskId"], task_after["id"]);
    assert_eq!(
        peer.request(
            "modelUsage/getSummary",
            json!({"scope":{"kind":"task","taskId":"missing"}})
        )["error"]["code"],
        -32004
    );
    assert_eq!(peer.request("modelUsage/getSummary", json!({"scope":{"kind":"session","sessionId":first["id"],"workspaceId":workspace["id"]}}))["error"]["code"], -32602);
    drop(peer);
    let mut peer = Peer::start(&db);
    assert_eq!(summary(&mut peer, &scope), after);
    assert_eq!(
        summary(&mut peer, &empty_scope)["totals"],
        totals(0, 0, 0, (0, 0), (0, 0))
    );
    drop(peer);
    fs::remove_dir_all(root).unwrap();
}
