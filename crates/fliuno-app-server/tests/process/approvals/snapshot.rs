use super::*;

const SNAPSHOT: &str = "approval/listPendingSnapshot";

fn pending(peer: &mut Peer, session: &Value, text: &str) -> Value {
    let task = peer.call(
        "task/start",
        json!({"sessionId":session["id"],"input":text}),
    );
    peer.wait_for_status(task["id"].as_str().unwrap(), "waiting_approval")
}

fn task_ids(page: &Value) -> Vec<&str> {
    page["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["taskId"].as_str().unwrap())
        .collect()
}

#[test]
fn snapshot_resume_tracks_resolved_cancelled_and_new_approvals_across_upgrade_and_restart() {
    let root = std::env::temp_dir().join(format!("fliuno-inbox-snapshot-{}", uuid::Uuid::new_v4()));
    fs::create_dir_all(root.join("foreign")).unwrap();
    let db = root.join("kernel.db");
    let mut peer = Peer::start(&db);
    let workspace = peer.call("workspace/open", json!({"root":root}));
    let foreign = peer.call("workspace/open", json!({"root":root.join("foreign")}));
    let s1 = peer.call("session/create", json!({"workspaceId":workspace["id"]}));
    let s2 = peer.call("session/create", json!({"workspaceId":workspace["id"]}));
    let other = peer.call("session/create", json!({"workspaceId":foreign["id"]}));
    let first = pending(&mut peer, &s1, "first");
    let outside = pending(&mut peer, &other, "outside");
    let deny = pending(&mut peer, &s2, "to deny after capture");
    let cancel = pending(&mut peer, &s1, "to cancel after capture");
    let keep = pending(&mut peer, &s2, "keep open");
    let before_upgrade = peer.call(
        "approval/listPending",
        json!({"workspaceId":workspace["id"],"limit":1}),
    );
    drop(peer);

    // A real v40 inbox had no interval timestamps, but already had its own row cursors.
    rusqlite::Connection::open(&db)
        .unwrap()
        .execute_batch(
            "BEGIN;
         DROP TRIGGER stamp_pending_approval;
         DROP TRIGGER retain_closed_approval;
         DROP INDEX pending_approvals_opened;
         ALTER TABLE pending_approvals DROP COLUMN opened_through;
         DROP TABLE closed_approvals;
         DROP TABLE approval_inbox_meta;
         PRAGMA user_version=40;
         COMMIT;",
        )
        .unwrap();
    let mut peer = Peer::start(&db);
    let subscription = peer.call(
        "events/setScope",
        json!({"workspaceId":workspace["id"],"watermarkOnly":true}),
    );
    let mut query = json!({"page":{"workspaceId":workspace["id"],"limit":1,"targetBytes":1024}});
    let initial = peer.call(SNAPSHOT, query.clone());
    assert_eq!(initial["page"], before_upgrade);
    assert_eq!(task_ids(&initial["page"]), &[first["id"].as_str().unwrap()]);
    assert_eq!(
        initial["snapshot"]["throughSequence"],
        subscription["throughSequence"]
    );
    let snapshot = initial["snapshot"].clone();
    let through = snapshot["throughSequence"].as_u64().unwrap();
    let after = initial["page"]["nextAfter"].clone();
    let too_old = json!({"page":{},"snapshot":{"throughSequence":through-1,"capturedAtUnixMs":snapshot["capturedAtUnixMs"]}});
    assert_eq!(peer.request(SNAPSHOT, too_old)["error"]["code"], -32602);
    assert_eq!(
        peer.request(SNAPSHOT, json!({"page":{"after":after}}))["error"]["code"],
        -32602
    );
    assert_eq!(
        peer.request(SNAPSHOT, json!({"page":{"workspaceId":"missing"}}))["error"]["code"],
        -32004
    );

    peer.call(
        "approval/respond",
        json!({"taskId":deny["id"],"approvalId":deny["pendingApproval"]["id"],"decision":"deny"}),
    );
    peer.call("task/cancel", json!({"taskId":cancel["id"]}));
    let newcomer = pending(&mut peer, &s1, "added after capture");
    drop(peer);

    let mut peer = Peer::start(&db);
    query["snapshot"] = snapshot.clone();
    query["page"]["after"] = after;
    for (task, more) in [(&deny, true), (&cancel, true), (&keep, false)] {
        let continued = peer.call(SNAPSHOT, query.clone());
        assert_eq!(continued["snapshot"], snapshot);
        assert_eq!(
            task_ids(&continued["page"]),
            &[task["id"].as_str().unwrap()]
        );
        assert_eq!(continued["page"]["hasMore"], more);
        query["page"]["after"] = continued["page"]["nextAfter"].clone();
    }
    let in_session = peer.call(
        SNAPSHOT,
        json!({"page":{"sessionId":s1["id"]},"snapshot":snapshot}),
    );
    assert_eq!(
        task_ids(&in_session["page"]),
        &[
            first["id"].as_str().unwrap(),
            cancel["id"].as_str().unwrap()
        ]
    );
    let historical_all = peer.call(SNAPSHOT, json!({"page":{},"snapshot":snapshot}));
    assert_eq!(
        task_ids(&historical_all["page"]),
        &[&first, &outside, &deny, &cancel, &keep].map(|task| task["id"].as_str().unwrap())
    );
    let current = peer.call(SNAPSHOT, json!({"page":{"workspaceId":workspace["id"]}}));
    assert_eq!(
        task_ids(&current["page"]),
        &[&first, &keep, &newcomer].map(|task| task["id"].as_str().unwrap())
    );
    assert!(current["snapshot"]["throughSequence"].as_u64().unwrap() > through);
    let replay = peer.call(
        "events/listWorkspace",
        json!({"workspaceId":workspace["id"],"after":through}),
    );
    for (task, event_kind) in [
        (&deny, "approval_resolved"),
        (&cancel, "task_cancelled"),
        (&newcomer, "approval_requested"),
    ] {
        assert!(
            replay["events"]
                .as_array()
                .unwrap()
                .iter()
                .any(|event| event["taskId"] == task["id"] && event["data"]["type"] == event_kind)
        );
    }
    for closed in [&deny, &cancel] {
        let detail = json!({"taskId":closed["id"],"approvalId":closed["pendingApproval"]["id"]});
        assert_eq!(
            peer.request("approval/get", detail)["error"]["code"],
            -32004
        );
    }
    drop(peer);
    fs::remove_dir_all(root).unwrap();
}
