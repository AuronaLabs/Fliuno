use super::*;
use fliuno_client::EventReplay;
use fliuno_protocol::{
    TaskChange, TaskChangesPage, TaskChangesParams, TaskSummaryBatchParams, TaskSummaryChangesPage,
    WorkSnapshotParams,
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
                recent_task_limit: None,
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

#[test]
fn task_summary_changes_catch_up_from_a_pinned_window_and_then_follow_live() {
    let root =
        std::env::temp_dir().join(format!("fliuno-summary-changes-{}", uuid::Uuid::new_v4()));
    fs::create_dir_all(root.join("foreign")).unwrap();
    let db = root.join("kernel.db");
    let mut peer = Peer::start(&db);
    let workspace = peer.call("workspace/open", json!({"root":root}))["id"].clone();
    let foreign = peer.call("workspace/open", json!({"root":root.join("foreign")}))["id"].clone();
    let start = |peer: &mut Peer, workspace: &Value| {
        let session = peer.call("session/create", json!({"workspaceId":workspace}))["id"].clone();
        let task = peer.call(
            "task/startSummary",
            json!({"sessionId":session,"input":"save a local artifact"}),
        )["id"]
            .as_str()
            .unwrap()
            .to_owned();
        peer.wait_for_status(&task, "waiting_approval");
        (session, task)
    };
    let (first_session, first) = start(&mut peer, &workspace);
    let (second_session, second) = start(&mut peer, &workspace);
    let (_, outside) = start(&mut peer, &foreign);
    peer.call(
        "session/setArchived",
        json!({"sessionId":second_session,"archived":true}),
    );
    let pinned = peer.call("events/getWatermark", json!({}))["throughSequence"]
        .as_u64()
        .unwrap();
    peer.call("task/cancelSummary", json!({"taskId":first}));
    let with_cancel = peer.call("events/getWatermark", json!({}))["throughSequence"]
        .as_u64()
        .unwrap();
    peer.call("task/cancelSummary", json!({"taskId":outside}));
    let now = peer.call("events/getWatermark", json!({}))["throughSequence"]
        .as_u64()
        .unwrap();
    let args = json!({"workspaceId":workspace,"through":pinned,"limit":2});
    let page: TaskSummaryChangesPage =
        serde_json::from_value(peer.call("events/listTaskSummaryChanges", args.clone())).unwrap();
    let old = peer.call("events/listTaskChanges", args);
    assert!(page.has_more);
    assert_eq!(page.next_after, old["nextAfter"].as_u64().unwrap());
    assert!(page.next_after < pinned && pinned < with_cancel && with_cancel < now);
    assert_eq!(page.through_sequence, pinned);
    assert_eq!(page.summary_through_sequence, now);
    assert_eq!(page.changes.len(), 1);
    assert_eq!(
        page.changes[0].sequence,
        old["changes"][0]["sequence"].as_u64().unwrap()
    );
    assert_eq!(page.changes[0].summary.id, first);
    assert_eq!(page.changes[0].summary.status, TaskStatus::Cancelled);
    let all = peer.call("events/listTaskSummaryChanges", json!({"through":pinned}));
    assert_eq!(all["changes"].as_array().unwrap().len(), 3);
    let archived = peer.call(
        "events/listTaskSummaryChanges",
        json!({"sessionId":second_session}),
    );
    assert_eq!(archived["changes"].as_array().unwrap().len(), 1);
    assert_eq!(archived["changes"][0]["summary"]["id"], second);
    let one = peer.call("events/listTaskSummaryChanges", json!({"taskId":first}));
    assert_eq!(one["changes"][0]["sequence"], with_cancel);
    assert_eq!(one["throughSequence"], now);
    let empty = peer.call(
        "events/listTaskSummaryChanges",
        json!({"workspaceId":workspace,"after":with_cancel}),
    );
    assert_eq!(
        empty,
        json!({"changes":[],"nextAfter":now,"summaryThroughSequence":now,"throughSequence":now,"hasMore":false})
    );
    for (params, code) in [
        (
            json!({"workspaceId":workspace,"sessionId":first_session}),
            -32602,
        ),
        (json!({"sessionId":"missing"}), -32004),
        (json!({"through":now+1}), -32602),
        (json!({"after":pinned,"through":0}), -32602),
        (json!({"limit":0}), -32602),
        (json!({"limit":65}), -32602),
    ] {
        assert_eq!(
            peer.request("events/listTaskSummaryChanges", params)["error"]["code"],
            code
        );
    }
    drop(peer);

    let (server, mut client) = super::ag_ui_run::connect(&db);
    assert_eq!(client.info().capabilities["taskSummaryChangeReplay"], true);
    assert_eq!(client.info().capabilities["maxTaskSummaryChangeEvents"], 64);
    let workspace_id = workspace.as_str().unwrap().to_owned();
    let mut replay = EventReplay::new(
        ReplayScope::Workspace(workspace_id.clone()),
        page.next_after,
        pinned,
    )
    .unwrap();
    let mut seen = vec![first.clone()];
    while let Some(page) = replay
        .next_task_summary_changes(&mut client, 1, |_| {})
        .unwrap()
    {
        assert_eq!(page.through_sequence, pinned);
        assert_eq!(page.summary_through_sequence, now);
        seen.extend(page.changes.into_iter().map(|c| c.summary.id));
    }
    assert_eq!(replay.cursor(), pinned);
    assert_eq!(seen.last(), Some(&second));
    assert!(seen.iter().all(|id| id == &first || id == &second));
    let mut catch_up =
        EventReplay::new(ReplayScope::Workspace(workspace_id.clone()), pinned, now).unwrap();
    let page = catch_up
        .next_task_summary_changes(&mut client, 64, |_| {})
        .unwrap()
        .unwrap();
    assert_eq!(page.next_after, now);
    assert_eq!(page.changes.len(), 1);
    assert_eq!(page.changes[0].sequence, with_cancel);
    assert_eq!(page.changes[0].summary.id, first);

    let mut work = client
        .subscribe_work(
            &WorkSnapshotParams {
                workspace_id: Some(workspace_id),
                session_id: None,
                approval_limit: 20,
                approval_target_bytes: None,
                recent_task_limit: None,
            },
            |_| {},
        )
        .unwrap();
    assert_eq!(work.events.cursor(), now);
    let _: TaskSummary = client
        .call(
            "task/cancelSummary",
            &TaskIdParams {
                task_id: second.clone(),
            },
            |note| {
                let _ = work.events.observe(note);
            },
        )
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    let page = loop {
        client
            .event_watermark(|note| {
                let _ = work.events.observe(note);
            })
            .unwrap();
        if let Some(page) = work
            .events
            .next_task_summary_changes(&mut client, 64, |_| {})
            .unwrap()
        {
            break page;
        }
        assert!(Instant::now() < deadline, "waiting for summary changes");
        thread::sleep(Duration::from_millis(5));
    };
    assert_eq!(page.changes.len(), 1);
    assert_eq!(page.changes[0].summary.id, second);
    assert_eq!(page.changes[0].summary.status, TaskStatus::Cancelled);
    assert_eq!(page.summary_through_sequence, work.events.cursor());
    assert_eq!(work.events.cursor(), now + 1);
    drop(client);
    drop(server);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn summary_revision_replay_crosses_upgrade_and_skips_unchanged_commits() {
    let root =
        std::env::temp_dir().join(format!("fliuno-summary-revisions-{}", uuid::Uuid::new_v4()));
    fs::create_dir_all(root.join("foreign")).unwrap();
    let db = root.join("kernel.db");
    let mut peer = Peer::start(&db);
    let workspace = peer.call("workspace/open", json!({"root":root}))["id"].clone();
    let foreign = peer.call("workspace/open", json!({"root":root.join("foreign")}))["id"].clone();
    let session = peer.call("session/create", json!({"workspaceId":workspace}))["id"].clone();
    let other = peer.call("session/create", json!({"workspaceId":foreign}))["id"].clone();
    let start = |peer: &mut Peer, session: &Value| {
        let task = peer.call(
            "task/start",
            json!({"sessionId":session,"input":"make an artifact"}),
        );
        peer.wait_for_status(task["id"].as_str().unwrap(), "waiting_approval")
    };
    let first = start(&mut peer, &session);
    let task_id = first["id"].as_str().unwrap();
    let cut = peer.call("events/getWatermark", json!({}))["throughSequence"]
        .as_u64()
        .unwrap();
    drop(peer);
    let sqlite = rusqlite::Connection::open(&db).unwrap();
    sqlite.execute_batch("DROP TABLE task_summary_revisions; DROP TABLE task_summary_revision_meta; PRAGMA user_version=44;").unwrap();
    let mut peer = Peer::start(&db);
    assert_eq!(sqlite.query_row::<(i64, i64),_,_>("SELECT legacy_through,(SELECT COUNT(*) FROM task_summary_revisions) FROM task_summary_revision_meta",[], |row| Ok((row.get(0)?,row.get(1)?))).unwrap(), (cut as i64,0));
    peer.call("approval/respond", json!({"taskId":task_id,"approvalId":first["pendingApproval"]["id"],"decision":"allow_once"}));
    peer.wait_for_status(task_id, "completed");
    peer.call(
        "session/setArchived",
        json!({"sessionId":session,"archived":true}),
    );
    let unrelated = start(&mut peer, &other);
    let through = peer.call("events/getWatermark", json!({}))["throughSequence"]
        .as_u64()
        .unwrap();
    let events = peer.call("events/list", json!({"taskId":task_id,"after":cut}));
    let rows = events["events"].as_array().unwrap();
    let selected = rows
        .iter()
        .filter(|row| {
            matches!(
                row["data"]["type"].as_str(),
                Some("approval_resolved" | "model_started" | "task_completed")
            )
        })
        .map(|row| row["sequence"].as_u64().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(selected.len(), 3);
    assert!(rows.len() > selected.len()); // Tool and artifact commits remain in the full log.

    let rpc = "events/listTaskSummaryRevisions";
    let before = json!({"workspaceId":workspace,"after":cut-1,"through":cut,"limit":1});
    assert_eq!(
        peer.call(rpc, before.clone()),
        peer.call("events/listTaskSummaryChanges", before)
    );
    let latest_rpc = "events/listLatestTaskSummaryRevisions";
    let latest = peer.call(
        latest_rpc,
        json!({"workspaceId":workspace,"after":cut-1,"through":through,"limit":1}),
    );
    assert_eq!(latest["changes"].as_array().unwrap().len(), 1);
    assert_eq!(latest["changes"][0]["sequence"], selected[2]);
    assert_eq!(latest["changes"][0]["summary"]["id"], task_id);
    assert_eq!(latest["nextAfter"], through);
    assert_eq!(latest["hasMore"], false);
    let latest_legacy = peer.call(
        latest_rpc,
        json!({"workspaceId":workspace,"after":0,"through":cut,"limit":1}),
    );
    assert_eq!(latest_legacy["changes"][0]["sequence"], cut);
    assert_eq!(latest_legacy["hasMore"], false);
    let first_page = peer.call(
        rpc,
        json!({"workspaceId":workspace,"after":cut-1,"through":through,"limit":1}),
    );
    assert_eq!(first_page["nextAfter"], cut);
    assert_eq!(first_page["hasMore"], true);
    let crossing = peer.call(
        rpc,
        json!({"workspaceId":workspace,"after":cut-1,"through":through,"limit":2}),
    );
    assert_eq!(crossing["nextAfter"], selected[0]);
    assert_eq!(crossing["changes"].as_array().unwrap().len(), 1);
    assert_eq!(crossing["changes"][0]["sequence"], selected[0]);
    assert_eq!(crossing["changes"][0]["summary"]["status"], "completed");
    let task_page = peer.call(rpc, json!({"taskId":task_id,"after":cut,"through":through}));
    assert_eq!(
        task_page,
        peer.call(
            rpc,
            json!({"sessionId":session,"after":cut,"through":through})
        )
    );
    assert_eq!(task_page["nextAfter"], through);
    assert_eq!(task_page["changes"][0]["sequence"], selected[2]);
    let all = peer.call(rpc, json!({"after":cut,"through":through}));
    assert_eq!(all["changes"][1]["summary"]["id"], unrelated["id"]);
    let legacy = peer.call(
        "events/listTaskSummaryChanges",
        json!({"taskId":task_id,"after":selected[0],"through":selected[1]-1}),
    );
    assert!(!legacy["changes"].as_array().unwrap().is_empty());
    let precise = peer.call(
        rpc,
        json!({"taskId":task_id,"after":selected[0],"through":selected[1]-1}),
    );
    assert!(precise["changes"].as_array().unwrap().is_empty());
    assert_eq!(precise["nextAfter"], selected[1] - 1);
    assert_eq!(precise["summaryThroughSequence"], through);
    drop(peer);
    drop(sqlite);

    let (server, mut client) = super::ag_ui_run::connect(&db);
    assert_eq!(
        client.info().capabilities["taskSummaryRevisionReplay"],
        true
    );
    let mut replay = EventReplay::new(
        ReplayScope::Workspace(workspace.as_str().unwrap().into()),
        cut,
        through,
    )
    .unwrap();
    for expected in selected {
        let page = replay
            .next_task_summary_revisions(&mut client, 1, |_| {})
            .unwrap()
            .unwrap();
        assert_eq!(page.changes[0].sequence, expected);
        assert_eq!(page.changes[0].summary.id, task_id);
    }
    assert_eq!(replay.cursor(), through);
    assert!(
        replay
            .next_task_summary_revisions(&mut client, 1, |_| {})
            .unwrap()
            .is_none()
    );

    let mut work = client
        .subscribe_work(
            &WorkSnapshotParams {
                workspace_id: Some(foreign.as_str().unwrap().into()),
                session_id: None,
                approval_limit: 1,
                approval_target_bytes: None,
                recent_task_limit: None,
            },
            |_| {},
        )
        .unwrap();
    let _: Value = client
        .call(
            "task/cancelSummary",
            &json!({"taskId":unrelated["id"]}),
            |note| {
                work.events.observe(note);
            },
        )
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    let live = loop {
        client
            .event_watermark(|note| {
                work.events.observe(note);
            })
            .unwrap();
        if let Some(page) = work
            .events
            .next_latest_task_summary_revisions(&mut client, 1, |_| {})
            .unwrap()
        {
            break page;
        }
        assert!(Instant::now() < deadline, "missing live revision");
        thread::sleep(Duration::from_millis(5));
    };
    assert_eq!(live.changes[0].summary.id, unrelated["id"]);
    assert_eq!(live.changes[0].summary.status, TaskStatus::Cancelled);
    drop(client);
    drop(server);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn latest_revision_replay_advances_empty_pages_and_pins_the_window_end() {
    let root = std::env::temp_dir().join(format!(
        "fliuno-coalesce-revisions-{}",
        uuid::Uuid::new_v4()
    ));
    fs::create_dir(&root).unwrap();
    let db = root.join("kernel.db");
    let mut peer = Peer::start(&db);
    let workspace = peer.call("workspace/open", json!({"root":root}))["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let session = peer.call("session/create", json!({"workspaceId":workspace}))["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let task = peer.call(
        "task/startSummary",
        json!({"sessionId":session,"input":"prepare a chart"}),
    )["id"]
        .as_str()
        .unwrap()
        .to_owned();
    peer.wait_for_status(&task, "waiting_approval");
    let after = peer.call("events/getWatermark", json!({}))["throughSequence"]
        .as_u64()
        .unwrap();
    drop(peer);

    // Seed a previously recorded hotspot without running thousands of model turns.
    let mut db_handle = rusqlite::Connection::open(&db).unwrap();
    let tx = db_handle.transaction().unwrap();
    let count = fliuno_protocol::MAX_TASK_CHANGE_EVENTS + 1;
    tx.execute(
        "WITH RECURSIVE copies(n) AS (VALUES(1) UNION ALL SELECT n+1 FROM copies WHERE n<?2)
         INSERT INTO events(task_id,data)
         SELECT ?1,(SELECT data FROM events WHERE task_id=?1 ORDER BY sequence LIMIT 1) FROM copies",
        rusqlite::params![task, count],
    ).unwrap();
    tx.execute(
        "INSERT INTO task_summary_revisions(sequence,task_id,session_id,workspace_id)
         SELECT sequence,task_id,?2,?3 FROM events WHERE task_id=?1 AND sequence>?4",
        rusqlite::params![task, session, workspace, after as i64],
    )
    .unwrap();
    tx.commit().unwrap();
    drop(db_handle);

    let (server, mut client) = super::ag_ui_run::connect(&db);
    assert_eq!(
        client.info().capabilities["latestTaskSummaryRevisionReplay"],
        true
    );
    let through = client.event_watermark(|_| {}).unwrap();
    assert_eq!(through, after + u64::from(count));
    let mut replay = EventReplay::new(ReplayScope::Session(session), after, through).unwrap();
    let empty = replay
        .next_latest_task_summary_revisions(&mut client, 1, |_| {})
        .unwrap()
        .unwrap();
    assert!(empty.changes.is_empty() && empty.has_more);
    assert_eq!(empty.next_after, through - 1);
    assert_eq!(replay.cursor(), empty.next_after);

    // A new durable revision is visible in the summary but belongs to a later window.
    let updated: TaskSummary = client
        .call("task/cancelSummary", &json!({"taskId":task}), |_| {})
        .unwrap();
    assert_eq!(updated.status, TaskStatus::Cancelled);
    let tail = replay
        .next_latest_task_summary_revisions(&mut client, 1, |_| {})
        .unwrap()
        .unwrap();
    assert_eq!(tail.changes.len(), 1);
    assert_eq!(tail.changes[0].sequence, through);
    assert_eq!(tail.changes[0].summary.id, task);
    assert_eq!(tail.changes[0].summary.status, TaskStatus::Cancelled);
    assert!(tail.summary_through_sequence > through);
    assert_eq!(replay.cursor(), through);
    assert!(!tail.has_more);

    let mut next = EventReplay::new(
        ReplayScope::Task(task),
        through,
        tail.summary_through_sequence,
    )
    .unwrap();
    let updated = next
        .next_latest_task_summary_revisions(&mut client, 1, |_| {})
        .unwrap()
        .unwrap();
    assert_eq!(updated.changes[0].sequence, tail.summary_through_sequence);
    drop(client);
    drop(server);
    fs::remove_dir_all(root).unwrap();
}
