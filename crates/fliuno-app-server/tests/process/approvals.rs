use super::*;

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
            .execute_batch("DROP TABLE pending_approvals; PRAGMA user_version=19;")
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
