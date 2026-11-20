use super::*;

#[path = "approvals/decisions.rs"]
mod decisions;

#[path = "approvals/snapshot.rs"]
mod snapshot;

#[test]
fn approval_run_binding_migrates_and_replays_one_decision_after_restart() {
    let root = std::env::temp_dir().join(format!("fliuno-approval-run-{}", uuid::Uuid::new_v4()));
    fs::create_dir(&root).unwrap();
    let path = root.join("kernel.db");
    let mut peer = Peer::start(&path);
    let workspace = peer.call("workspace/open", json!({"root":root}));
    let session = peer.call("session/create", json!({"workspaceId":workspace["id"]}));
    let other = peer.call("session/create", json!({"workspaceId":workspace["id"]}));
    let start =
        json!({"sessionId":session["id"],"input":"build one artifact","clientRequestId":"parent"});
    let task = peer.call("task/startSummary", start.clone());
    let task_id = task["id"].as_str().unwrap();
    let waiting = peer.wait_for_status(task_id, "waiting_approval");
    let approval = &waiting["pendingApproval"]["id"];
    drop(peer);

    let db = rusqlite::Connection::open(&path).unwrap();
    db.execute_batch("DROP TABLE task_run_ends; DROP TABLE approval_runs; PRAGMA user_version=37;")
        .unwrap();
    drop(db);
    let mut peer = Peer::start(&path);
    let parent = json!({"sessionId":session["id"],"clientRequestId":"parent"});
    let child = json!({"sessionId":session["id"],"clientRequestId":"child"});
    let original = peer.call("approval/getRunWindow", parent.clone());
    let first = peer.call("events/get", json!({"sequence":original["firstSequence"]}));
    assert_eq!(first["data"]["type"], "task_started");
    let boundary = peer.call("events/get", json!({"sequence":original["lastSequence"]}));
    assert_eq!(boundary["data"]["approval"]["id"], *approval);
    assert_eq!(
        peer.call("approval/getRun", parent.clone())["startSequence"],
        Value::Null
    );
    let request = json!({"sessionId":session["id"],"parentRequestId":"parent",
        "clientRequestId":"child","approvalId":approval,"decision":"allow_once"});
    for (field, value, code) in [
        ("sessionId", other["id"].clone(), -32004),
        ("parentRequestId", json!("other"), -32004),
        ("approvalId", json!("other"), -32004),
        ("clientRequestId", json!("parent"), -32009),
    ] {
        let mut invalid = request.clone();
        invalid[field] = value;
        assert_eq!(
            peer.request("approval/resumeRun", invalid)["error"]["code"],
            code
        );
    }
    let accepted = peer.call("approval/resumeRun", request.clone());
    assert_eq!(accepted["task"]["id"], task_id);
    let sequence = &accepted["startSequence"];
    let decision = peer.call("events/get", json!({"sequence":sequence}));
    assert_eq!(
        decision["data"],
        json!({"type":"approval_resolved","approvalId":approval,"decision":"allow_once"})
    );
    assert_eq!(
        peer.request("approval/cancelRun", parent.clone())["error"]["code"],
        -32009
    );
    peer.wait_for_status(task_id, "completed");
    let resumed = peer.call("approval/getRunWindow", child.clone());
    assert_eq!(resumed["firstSequence"], *sequence);
    let completed = peer.call("events/get", json!({"sequence":resumed["lastSequence"]}));
    assert_eq!(completed["data"]["type"], "task_completed");
    drop(peer);

    // Existing v38 databases already contain parent and child bindings.
    let db = rusqlite::Connection::open(&path).unwrap();
    db.execute_batch("DROP TABLE task_run_ends; PRAGMA user_version=38;")
        .unwrap();
    drop(db);

    let mut peer = Peer::start(&path);
    assert_eq!(peer.call("approval/getRunWindow", parent), original);
    assert_eq!(peer.call("approval/getRunWindow", child.clone()), resumed);
    assert_eq!(
        peer.call("approval/getRun", child)["startSequence"],
        *sequence
    );
    let retry = peer.call("approval/resumeRun", request.clone());
    assert_eq!(retry["startSequence"], *sequence);
    assert_eq!(retry["task"]["status"], "completed");
    for (field, value) in [
        ("decision", json!("deny")),
        ("parentRequestId", json!("other")),
        ("approvalId", json!("other")),
    ] {
        let mut conflict = request.clone();
        conflict[field] = value;
        assert_eq!(
            peer.request("approval/resumeRun", conflict)["error"]["code"],
            -32009
        );
    }
    let mut duplicate = start;
    duplicate["clientRequestId"] = json!("child");
    assert_eq!(
        peer.request("task/startSummary", duplicate)["error"]["code"],
        -32009
    );
    let page = peer.call("events/list", json!({"taskId":task_id}));
    let events = page["events"].as_array().unwrap();
    for kind in ["approval_resolved", "tool_started", "artifact_created"] {
        assert_eq!(
            events
                .iter()
                .filter(|event| event["data"]["type"] == kind)
                .count(),
            1
        );
    }
    drop(peer);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn approval_resume_rejects_a_live_approval_outside_the_parent_boundary() {
    use http_wire::{Fixture, Response};
    use std::sync::atomic::{AtomicUsize, Ordering};

    let step = AtomicUsize::new(0);
    let model = Fixture::start(move |_| {
        let step = step.fetch_add(1, Ordering::Relaxed);
        let arguments = json!({"title":format!("artifact {step}"),"content":"content"});
        Response::json(
            200,
            json!({"status":"completed","output":[{
                "type":"function_call","call_id":format!("call-{step}"),
                "name":"artifact_create","arguments":arguments.to_string()
            }]}),
        )
    });
    let root = std::env::temp_dir().join(format!("fliuno-run-boundary-{}", uuid::Uuid::new_v4()));
    fs::create_dir(&root).unwrap();
    let path = root.join("kernel.db");
    let url = model.url("/v1/");
    let mut peer = Peer::start_with_model_fixture(&path, Some(&url));
    let workspace = peer.call("workspace/open", json!({"root":root}));
    let session = peer.call("session/create", json!({"workspaceId":workspace["id"]}));
    let task = peer.call(
        "task/startSummary",
        json!({"sessionId":session["id"],"input":"two pauses","clientRequestId":"parent"}),
    );
    let task_id = task["id"].as_str().unwrap();
    let first = peer.wait_for_status(task_id, "waiting_approval");
    peer.call(
        "approval/respond",
        json!({
            "taskId":task_id,"approvalId":first["pendingApproval"]["id"],"decision":"allow_once"
        }),
    );
    let second = peer.wait_for_status(task_id, "waiting_approval");
    let approval = &second["pendingApproval"]["id"];
    assert_ne!(*approval, first["pendingApproval"]["id"]);
    let scope = json!({"taskId":task_id});
    let before = peer.call("events/list", scope.clone());
    drop(peer);
    let mut peer = Peer::start_with_model_fixture(&path, Some(&url));
    assert_eq!(
        peer.request(
            "approval/resumeRun",
            json!({
                "sessionId":session["id"],"parentRequestId":"parent","clientRequestId":"child",
                "approvalId":approval,"decision":"deny"
            })
        )["error"]["code"],
        -32009
    );
    assert_eq!(peer.call("events/list", scope.clone()), before);
    assert_eq!(peer.call("task/get", scope), second);
    assert_eq!(model.requests().len(), 2);
    drop(peer);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn pending_approval_inbox_migrates_preserves_cursors_and_tracks_resolution() {
    let directory = std::env::temp_dir()
        .canonicalize()
        .unwrap()
        .join(format!("fliuno-approvals-{}", uuid::Uuid::new_v4()));
    let workspace_a = directory.join("a");
    let workspace_b = directory.join("b");
    fs::create_dir_all(&workspace_a).unwrap();
    fs::create_dir_all(&workspace_b).unwrap();
    let db = directory.join("kernel.db");
    let mut peer = Peer::start(&db);
    let a = peer.call("workspace/open", json!({"root":workspace_a}));
    let b = peer.call("workspace/open", json!({"root":workspace_b}));
    let a1 = peer.call("session/create", json!({"workspaceId":a["id"]}));
    let b1 = peer.call("session/create", json!({"workspaceId":b["id"]}));
    let a2 = peer.call("session/create", json!({"workspaceId":a["id"]}));
    let mut start = |session: &Value, input: &str| {
        let task = peer.call(
            "task/start",
            json!({"sessionId":session["id"],"input":input}),
        );
        peer.wait_for_status(task["id"].as_str().unwrap(), "waiting_approval")
    };
    let first = start(&a1, "first");
    let other = start(&b1, "other workspace");
    let last = start(&a2, "last");
    drop(peer);
    {
        let legacy = rusqlite::Connection::open(&db).unwrap();
        legacy
            .execute_batch("DROP TABLE pending_approvals; DROP TABLE closed_approvals; DROP TABLE approval_inbox_meta; PRAGMA user_version=19;")
            .unwrap();
    }

    let mut peer = Peer::start(&db);
    let scoped = peer.call(
        "approval/listPending",
        json!({"workspaceId":a["id"],"limit":1}),
    );
    let item = json!({"id":first["pendingApproval"]["id"],"taskId":first["id"],"sessionId":a1["id"],
        "kind":"tool_call","allowOnceAvailable":true,"expiresAtUnixMs":null});
    assert_eq!(scoped["items"], json!([item]));
    assert_eq!(scoped["hasMore"], true);
    let details = json!({"taskId":first["id"],"approvalId":item["id"]});
    assert_eq!(
        peer.call("approval/get", details.clone()),
        json!({
            "kind":"tool_call","approval":first["pendingApproval"]
        })
    );
    assert_eq!(
        peer.request(
            "approval/get",
            json!({
                "taskId":other["id"],"approvalId":item["id"]
            })
        )["error"]["code"],
        -32004
    );
    assert_eq!(
        peer.call("approval/listPending", json!({"sessionId":a1["id"]}))["items"],
        json!([item])
    );
    assert_eq!(
        peer.call("approval/listPending", json!({}))["items"][1]["taskId"],
        other["id"]
    );
    for invalid in [
        json!({"limit":0}),
        json!({"limit":101}),
        json!({"after":u64::MAX}),
        json!({"workspaceId":a["id"],"sessionId":a1["id"]}),
    ] {
        assert_eq!(
            peer.request("approval/listPending", invalid)["error"]["code"],
            -32602
        );
    }
    assert_eq!(
        peer.request("approval/listPending", json!({"sessionId":"unknown"}))["error"]["code"],
        -32004
    );
    peer.call(
        "approval/respond",
        json!({"taskId":first["id"],"approvalId":item["id"],"decision":"deny"}),
    );
    assert_eq!(
        peer.request("approval/get", details)["error"]["code"],
        -32004
    );
    peer.call("task/cancel", json!({"taskId":other["id"]}));
    drop(peer);

    let mut peer = Peer::start(&db);
    let remaining = peer.call(
        "approval/listPending",
        json!({"workspaceId":a["id"],"after":scoped["nextAfter"]}),
    );
    assert_eq!(remaining["items"].as_array().unwrap().len(), 1);
    assert_eq!(remaining["items"][0]["id"], last["pendingApproval"]["id"]);
    assert_eq!(remaining["hasMore"], false);
    peer.call("task/cancel", json!({"taskId":last["id"]}));
    assert_eq!(
        peer.call("approval/listPending", json!({}))["items"],
        json!([])
    );
    let next = peer.call(
        "task/start",
        json!({"sessionId":a2["id"],"input":"new task after the inbox became empty"}),
    );
    let next = peer.wait_for_status(next["id"].as_str().unwrap(), "waiting_approval");
    let new_page = peer.call(
        "approval/listPending",
        json!({"after":remaining["nextAfter"]}),
    );
    assert_eq!(new_page["items"][0]["id"], next["pendingApproval"]["id"]);
    drop(peer);
    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn pending_approval_pages_respect_byte_targets_and_resume_after_restart() {
    let directory = std::env::temp_dir()
        .canonicalize()
        .unwrap()
        .join(format!("fliuno-approval-pages-{}", uuid::Uuid::new_v4()));
    fs::create_dir_all(&directory).unwrap();
    let db = directory.join("kernel.db");
    let mut peer = Peer::start(&db);
    let workspace = peer.call("workspace/open", json!({"root":directory}));
    let session = peer.call("session/create", json!({"workspaceId":workspace["id"]}));
    for index in 0..12 {
        let task = peer.call(
            "task/start",
            json!({"sessionId":session["id"],"input":format!("approval {index}")}),
        );
        peer.wait_for_status(task["id"].as_str().unwrap(), "waiting_approval");
    }
    let full = peer.call(
        "approval/listPending",
        json!({"workspaceId":workspace["id"]}),
    );
    let expected = full["items"].as_array().unwrap();
    assert_eq!(expected.len(), 12);
    let count = (1..expected.len())
        .find(|count| serde_json::to_vec(&expected[..*count]).unwrap().len() >= 1024)
        .unwrap();
    let target = serde_json::to_vec(&expected[..count]).unwrap().len() as u32;
    let first = peer.call(
        "approval/listPending",
        json!({"workspaceId":workspace["id"],"targetBytes":target}),
    );
    assert_eq!(first["items"].as_array().unwrap(), &expected[..count]);
    assert_eq!(first["hasMore"], true);
    drop(peer);

    let mut peer = Peer::start(&db);
    let second = peer.call(
        "approval/listPending",
        json!({"sessionId":session["id"],"after":first["nextAfter"],"targetBytes":target}),
    );
    assert_eq!(
        second["items"].as_array().unwrap(),
        &expected[count..count * 2]
    );
    let invalid = peer.request(
        "approval/listPending",
        json!({"workspaceId":workspace["id"],"targetBytes":1023}),
    );
    assert_eq!(invalid["error"]["code"], -32602);
    drop(peer);
    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn approval_decisions_can_be_reconciled_after_reconnect_and_v33_backfill() {
    let root = std::env::temp_dir().canonicalize().unwrap().join(format!(
        "fliuno-approval-decisions-{}",
        uuid::Uuid::new_v4()
    ));
    fs::create_dir(&root).unwrap();
    let db = root.join("kernel.db");
    let mut peer = Peer::start(&db);
    let workspace = peer.call("workspace/open", json!({"root":root}));
    let session = peer.call("session/create", json!({"workspaceId":workspace["id"]}));
    let mut receipts = Vec::new();
    for (decision, status) in [("deny", "failed"), ("allow_once", "completed")] {
        let task = peer.call(
            "task/start",
            json!({"sessionId":session["id"],"input":decision}),
        );
        let id = task["id"].as_str().unwrap();
        let waiting = peer.wait_for_status(id, "waiting_approval");
        let lookup = json!({"taskId":id,"approvalId":waiting["pendingApproval"]["id"]});
        assert_eq!(
            peer.request("approval/getDecision", lookup.clone())["error"]["code"],
            -32004
        );
        let mut params = lookup.clone();
        params["decision"] = json!(decision);
        let direct = if decision == "allow_once" {
            Some(peer.call("approval/respondDecision", params.clone()))
        } else {
            peer.call("approval/respond", params.clone());
            None
        };
        peer.wait_for_status(id, status);
        let receipt = peer.call("approval/getDecision", lookup.clone());
        if let Some(direct) = direct {
            assert_eq!(direct, receipt);
        }
        let before_retry = peer.call("events/getWatermark", json!({}));
        assert_eq!(
            peer.call("approval/respondDecision", params.clone()),
            receipt
        );
        let mut conflicting = params;
        conflicting["decision"] = json!(if decision == "allow_once" {
            "deny"
        } else {
            "allow_once"
        });
        assert_eq!(
            peer.request("approval/respondDecision", conflicting)["error"]["code"],
            -32009
        );
        assert_eq!(peer.call("events/getWatermark", json!({})), before_retry);
        assert_eq!(receipt["taskId"], id);
        assert_eq!(
            receipt["data"],
            json!({"type":"approval_resolved","approvalId":lookup["approvalId"],"decision":decision})
        );
        assert_eq!(
            peer.call("events/get", json!({"sequence":receipt["sequence"]})),
            receipt
        );
        assert_eq!(
            peer.request("approval/get", lookup.clone())["error"]["code"],
            -32004
        );
        receipts.push((lookup, receipt));
    }
    let different_task =
        json!({"taskId":receipts[0].0["taskId"],"approvalId":receipts[1].0["approvalId"]});
    assert_eq!(
        peer.request("approval/getDecision", different_task)["error"]["code"],
        -32004
    );
    assert_eq!(
        peer.request(
            "approval/getDecision",
            json!({"taskId":receipts[0].0["taskId"],"approvalId":""})
        )["error"]["code"],
        -32602
    );
    drop(peer);
    {
        let legacy = rusqlite::Connection::open(&db).unwrap();
        legacy
            .execute_batch("DROP TABLE approval_decisions; PRAGMA user_version=33;")
            .unwrap();
    }
    let mut peer = Peer::start(&db);
    for (lookup, receipt) in receipts {
        assert_eq!(peer.call("approval/getDecision", lookup.clone()), receipt);
        let mut params = lookup;
        params["decision"] = receipt["data"]["decision"].clone();
        assert_eq!(peer.call("approval/respondDecision", params), receipt);
    }
    drop(peer);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn historical_pending_approval_pages_keep_pre_upgrade_bounds_across_scopes() {
    let root =
        std::env::temp_dir().join(format!("fliuno-approval-window-{}", uuid::Uuid::new_v4()));
    fs::create_dir_all(root.join("foreign")).unwrap();
    let db = root.join("kernel.db");
    let mut peer = Peer::start(&db);
    let workspace = peer.call("workspace/open", json!({"root":root}));
    let foreign = peer.call("workspace/open", json!({"root":root.join("foreign")}));
    let first = peer.call("session/create", json!({"workspaceId":workspace["id"]}));
    let sibling = peer.call("session/create", json!({"workspaceId":workspace["id"]}));
    let outside = peer.call("session/create", json!({"workspaceId":foreign["id"]}));
    let submit = |peer: &mut Peer, session: &Value| {
        let task = peer.call(
            "task/startSummary",
            json!({"sessionId":session["id"],"input":"review"}),
        );
        peer.wait_for_status(task["id"].as_str().unwrap(), "waiting_approval")
    };
    let head = submit(&mut peer, &first);
    let older = submit(&mut peer, &first);
    submit(&mut peer, &outside);
    let sibling_task = submit(&mut peer, &sibling);
    let newest = submit(&mut peer, &first);
    let method = "approval/listPendingSnapshot";
    let scopes = [
        json!({}),
        json!({"workspaceId":workspace["id"]}),
        json!({"sessionId":first["id"]}),
    ];
    let mut pages = Vec::new();
    for scope in &scopes {
        let mut page = scope.clone();
        page["limit"] = json!(1);
        pages.push(peer.call(method, json!({"page":page})));
    }
    // Closing approvals out of order changes their storage table but not the
    // opening fence. The newest saved row opened exactly at the captured waterline.
    for task in [&newest, &older] {
        peer.call("approval/respond", json!({"taskId":task["id"],"approvalId":task["pendingApproval"]["id"],"decision":"deny"}));
        peer.wait_for_status(task["id"].as_str().unwrap(), "failed");
    }
    let future = submit(&mut peer, &first);
    drop(peer);
    rusqlite::Connection::open(&db).unwrap().execute_batch(
        "DROP INDEX pending_approvals_opened; DROP INDEX closed_approvals_opened; PRAGMA user_version=51;"
    ).unwrap();
    let mut peer = Peer::start(&db);
    for (index, (scope, snapshot)) in scopes.iter().zip(pages).enumerate() {
        assert_eq!(snapshot["page"]["items"][0]["taskId"], head["id"]);
        let mut page = scope.clone();
        page["after"] = snapshot["page"]["nextAfter"].clone();
        let after = peer.call(method, json!({"page":page,"snapshot":snapshot["snapshot"]}));
        assert_eq!(after["snapshot"], snapshot["snapshot"]);
        let items = after["page"]["items"].as_array().unwrap();
        assert_eq!(items.first().unwrap()["taskId"], older["id"]);
        assert_eq!(items.last().unwrap()["taskId"], newest["id"]);
        assert_eq!(items.len(), 4 - index);
        assert_eq!(
            items.iter().any(|row| row["taskId"] == sibling_task["id"]),
            index < 2
        );
        assert!(!items.iter().any(|row| row["taskId"] == future["id"]));
        assert_eq!(after["page"]["hasMore"], false);
        page["after"] = after["page"]["nextAfter"].clone();
        let terminal = peer.call(method, json!({"page":page,"snapshot":snapshot["snapshot"]}));
        assert_eq!(
            terminal["page"],
            json!({"items":[],"nextAfter":page["after"],"hasMore":false})
        );
    }
    drop(peer);
    fs::remove_dir_all(root).unwrap();
}
