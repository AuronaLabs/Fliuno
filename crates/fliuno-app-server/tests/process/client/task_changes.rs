use super::*;
use fliuno_client::EventReplay;
use fliuno_protocol::{
    TaskChange, TaskChangesPage, TaskChangesParams, TaskSummaryBatchParams, WorkSnapshotParams,
};

#[test]
fn task_change_replay_keeps_scope_pages_and_watermarks_across_restart() {
    let root = std::env::temp_dir().join(format!("fliuno-task-changes-{}", uuid::Uuid::new_v4()));
    fs::create_dir_all(root.join("foreign")).unwrap();
    let db = root.join("kernel.db");
    let mut peer = Peer::start(&db);
    let workspace = peer.call("workspace/open", json!({"root":root}));
    let foreign = peer.call("workspace/open", json!({"root":root.join("foreign")}));
    let make_session = |peer: &mut Peer, workspace: &Value| {
        peer.call("session/create", json!({"workspaceId":workspace["id"]}))["id"]
            .as_str()
            .unwrap()
            .to_owned()
    };
    let first = make_session(&mut peer, &workspace);
    let sibling = make_session(&mut peer, &workspace);
    let other = make_session(&mut peer, &foreign);
    let start = |peer: &mut Peer, session: &str, input: &str| {
        let task = peer.call(
            "task/startSummary",
            json!({"sessionId":session,"input":input,"context":{"mode":"isolated"}}),
        );
        let id = task["id"].as_str().unwrap().to_owned();
        peer.wait_for_status(&id, "waiting_approval");
        id
    };
    let a = start(&mut peer, &first, &"z".repeat(128 * 1024));
    let outside = start(&mut peer, &other, "foreign artifact");
    let b = start(&mut peer, &sibling, "second artifact");
    peer.call(
        "session/setArchived",
        json!({"sessionId":sibling,"archived":true}),
    );
    let workspace_id = workspace["id"].as_str().unwrap();
    let scoped = |peer: &mut Peer, method: &str, options| {
        let mut params = json!({"workspaceId":workspace_id});
        params.as_object_mut().unwrap().extend(options);
        peer.call(method, params)
    };
    let full = scoped(
        &mut peer,
        "events/listWorkspace",
        [("limit".into(), json!(100))],
    );
    let raw = full["events"].as_array().unwrap();
    assert!(raw.len() > 3);
    assert!(serde_json::to_vec(&full).unwrap().len() > 128 * 1024);
    let page: TaskChangesPage = serde_json::from_value(scoped(
        &mut peer,
        "events/listTaskChanges",
        [("limit".into(), json!(2))],
    ))
    .unwrap();
    assert!(page.has_more);
    assert_eq!(
        page.changes,
        [TaskChange {
            task_id: a.clone(),
            sequence: raw[1]["sequence"].as_u64().unwrap()
        }]
    );
    assert_eq!(page.next_after, page.changes[0].sequence);
    let pinned = page.through_sequence;
    let request = |after, through, limit| TaskChangesParams {
        task_id: None,
        session_id: None,
        workspace_id: Some(workspace_id.into()),
        after,
        through,
        limit,
    };
    let compact = peer.call(
        "events/listTaskChanges",
        serde_json::to_value(request(0, Some(pinned), 100)).unwrap(),
    );
    assert!(serde_json::to_vec(&compact).unwrap().len() < 400);
    assert_eq!(compact["changes"].as_array().unwrap().len(), 2);
    assert_eq!(compact["changes"][0]["taskId"], a);
    assert_eq!(compact["changes"][1]["taskId"], b);
    assert_eq!(compact["nextAfter"], pinned);
    let foreign_task = peer.call(
        "events/listTaskChanges",
        json!({"taskId":outside,"through":pinned}),
    );
    assert_eq!(foreign_task["changes"][0]["taskId"], outside);
    let global = peer.call("events/listTaskChanges", json!({"through":pinned}));
    assert_eq!(global["changes"].as_array().unwrap().len(), 3);
    for (args, code) in [
        (json!({"sessionId":first,"taskId":a}), -32602),
        (json!({"workspaceId":"missing"}), -32004),
        (json!({"after":1,"through":0}), -32602),
        (json!({"through":pinned+1}), -32602),
        (json!({"limit":0}), -32602),
        (json!({"limit":1001}), -32602),
    ] {
        assert_eq!(
            peer.request("events/listTaskChanges", args)["error"]["code"],
            code
        );
    }
    peer.call("task/cancelSummary", json!({"taskId":outside}));
    drop(peer);

    let (server, mut client) = super::ag_ui_run::connect(&db);
    assert_eq!(client.info().capabilities["taskChangeReplay"], true);
    let mut replay = EventReplay::new(
        ReplayScope::Workspace(workspace_id.into()),
        page.next_after,
        pinned,
    )
    .unwrap();
    let mut scanned = Vec::new();
    while let Some(changes) = replay.next_task_changes(&mut client, 2, |_| {}).unwrap() {
        scanned.extend(changes);
    }
    assert_eq!(replay.cursor(), pinned);
    let last_by_task = scanned
        .iter()
        .fold(std::collections::BTreeMap::new(), |mut acc, change| {
            acc.insert(change.task_id.as_str(), change.sequence);
            acc
        });
    assert_eq!(last_by_task.len(), 2);
    assert_eq!(
        last_by_task[a.as_str()],
        compact["changes"][0]["sequence"].as_u64().unwrap()
    );
    assert_eq!(
        last_by_task[b.as_str()],
        compact["changes"][1]["sequence"].as_u64().unwrap()
    );
    let mut empty = EventReplay::new(
        ReplayScope::Workspace(workspace_id.into()),
        pinned,
        pinned + 1,
    )
    .unwrap();
    assert!(
        empty
            .next_task_changes(&mut client, 2, |_| {})
            .unwrap()
            .is_none()
    );
    assert_eq!(empty.cursor(), pinned + 1);

    let mut view = client
        .subscribe_work(
            &WorkSnapshotParams {
                workspace_id: None,
                session_id: Some(first.clone()),
                approval_limit: 20,
                approval_target_bytes: None,
            },
            |_| {},
        )
        .unwrap();
    let base = view.events.cursor();
    let _: TaskSummary = client
        .call(
            "task/cancelSummary",
            &TaskIdParams { task_id: a.clone() },
            |note| {
                let _ = view.events.observe(note);
            },
        )
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    let changes = loop {
        let _ = client
            .event_watermark(|note| {
                let _ = view.events.observe(note);
            })
            .unwrap();
        if let Some(changes) = view
            .events
            .next_task_changes(&mut client, 2, |_| {})
            .unwrap()
        {
            break changes;
        }
        assert!(Instant::now() < deadline, "waiting for task changes");
        thread::sleep(Duration::from_millis(5));
    };
    assert_eq!(
        changes,
        [TaskChange {
            task_id: a,
            sequence: base + 1
        }]
    );
    let summaries = client
        .get_task_summaries(
            &TaskSummaryBatchParams {
                task_ids: changes.into_iter().map(|change| change.task_id).collect(),
                workspace_id: None,
                session_id: Some(first),
            },
            |_| {},
        )
        .unwrap();
    assert_eq!(
        summaries.items[0].as_ref().unwrap().status,
        TaskStatus::Cancelled
    );
    assert_eq!(summaries.through_sequence, view.events.cursor());
    drop(client);
    drop(server);
    fs::remove_dir_all(root).unwrap();
}
