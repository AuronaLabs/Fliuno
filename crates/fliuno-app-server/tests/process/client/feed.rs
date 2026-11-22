use super::*;
use fliuno_protocol::{EventsPage, WorkSnapshotParams};
use std::{cell::RefCell, io, rc::Rc};

// Submit one notification batch in front of a normal RPC request. The batch
// commits several changes before the server can write another notification.
struct BatchWriter {
    stdin: ChildStdin,
    queued: Rc<RefCell<Option<Vec<u8>>>>,
}

impl Write for BatchWriter {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        if let Some(batch) = self.queued.borrow_mut().take() {
            self.stdin.write_all(&batch)?;
        }
        self.stdin.write(data)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.stdin.flush()
    }
}

fn wait_for_watermark<R: BufRead, W: Write>(
    client: &mut Client<R, W>,
    feed: &mut fliuno_client::EventFeed,
    workspace_id: &str,
) -> Vec<u64> {
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut watermarks = Vec::new();
    while watermarks.is_empty() {
        let _: Value = client
            .call(
                "workspace/get",
                &json!({"workspaceId":workspace_id}),
                |note| {
                    assert_ne!(note.method, "notifications/event");
                    assert_ne!(note.method, "notifications/eventGap");
                    if note.method == "notifications/eventWatermark" {
                        assert_eq!(note.params.as_object().unwrap().len(), 1);
                        watermarks.push(note.params["throughSequence"].as_u64().unwrap());
                    }
                    let _ = feed.observe(note);
                },
            )
            .unwrap();
        assert!(
            !watermarks.is_empty() || Instant::now() < deadline,
            "missing watermarks"
        );
        if watermarks.is_empty() {
            thread::sleep(Duration::from_millis(5));
        }
    }
    watermarks
}

#[test]
fn work_feed_coalesces_a_burst_and_replays_every_matching_commit() {
    let root = std::env::temp_dir().join(format!("fliuno-rust-feed-{}", uuid::Uuid::new_v4()));
    fs::create_dir(&root).unwrap();
    let db = root.join("kernel.db");
    // All tasks are waiting before the burst, so no background model commits
    // interfere with measuring exactly one wake for the framed batch.
    let mut peer = Peer::start(&db);
    let workspace = peer.call("workspace/open", json!({"root":root}));
    let session = peer.call("session/create", json!({"workspaceId":workspace["id"]}));
    let foreign = peer.call("session/create", json!({"workspaceId":workspace["id"]}));
    let start = |peer: &mut Peer, session: &Value, input: String| {
        let task = peer.call(
            "task/startSummary",
            json!({"sessionId":session["id"],"input":input,"context":{"mode":"isolated"}}),
        );
        let id = task["id"].as_str().unwrap().to_owned();
        peer.wait_for_status(&id, "waiting_approval");
        id
    };
    const MATCHING: usize = 72;
    let mut ids = (0..=MATCHING)
        .map(|i| start(&mut peer, &session, format!("artifact {i}")))
        .collect::<Vec<_>>();
    let next = ids.pop().unwrap();
    ids.insert(
        MATCHING / 2,
        start(&mut peer, &foreign, "foreign artifact".into()),
    );
    drop(peer);

    let queued = Rc::new(RefCell::new(None));
    let (mut server, mut client) =
        super::ag_ui_run::connect_with_writer(&db, |stdin| BatchWriter {
            stdin,
            queued: Rc::clone(&queued),
        });
    assert_eq!(client.info().capabilities["coalescedEventWatermarks"], true);
    let session_id = session["id"].as_str().unwrap();
    let workspace_id = workspace["id"].as_str().unwrap();
    let view = client
        .subscribe_work(
            &WorkSnapshotParams {
                workspace_id: None,
                session_id: Some(session_id.into()),
                approval_limit: 10,
                approval_target_bytes: None,
                recent_task_limit: None,
                include_actions: false,
            },
            |_| {},
        )
        .unwrap();
    assert_eq!(
        view.snapshot.task_counts.waiting_approval,
        (MATCHING + 1) as u64
    );
    let mut feed = view.events;
    let base = feed.cursor();
    assert!(
        feed.next_page(&mut client, 10, None, |_| {})
            .unwrap()
            .is_none()
    );

    let batch = ids
        .iter()
        .map(|id| json!({"jsonrpc":"2.0","method":"task/cancelSummary","params":{"taskId":id}}))
        .collect::<Vec<_>>();
    let mut frame = serde_json::to_vec(&batch).unwrap();
    frame.push(b'\n');
    *queued.borrow_mut() = Some(frame);
    let through = base + ids.len() as u64;
    assert_eq!(
        wait_for_watermark(&mut client, &mut feed, workspace_id),
        [through]
    );
    let mut delivered = Vec::new();
    while let Some(page) = feed
        .next_page(&mut client, 11, Some(4096), |note| {
            assert_ne!(note.method, "notifications/eventGap");
        })
        .unwrap()
    {
        assert!(!page.is_empty() && page.len() <= 11);
        delivered.extend(page);
    }
    assert_eq!(feed.cursor(), through);
    let exact: EventsPage = client
        .call(
            "events/listSession",
            &json!({
                "sessionId":session_id,"after":base,"through":through,"limit":100,
            }),
            |_| {},
        )
        .unwrap();
    assert_eq!(
        serde_json::to_value(&delivered).unwrap(),
        serde_json::to_value(exact.events).unwrap()
    );
    assert_eq!(delivered.len(), MATCHING);
    assert!(
        delivered
            .iter()
            .all(|event| matches!(event.data, EventKind::TaskCancelled))
    );
    assert!(
        !delivered
            .iter()
            .any(|event| event.task_id == ids[MATCHING / 2])
    );

    let mut after_burst = Vec::new();
    let _: TaskSummary = client
        .call(
            "task/cancelSummary",
            &TaskIdParams {
                task_id: next.clone(),
            },
            |note| {
                if note.method == "notifications/eventWatermark" {
                    after_burst.push(note.params["throughSequence"].as_u64().unwrap());
                }
                let _ = feed.observe(note);
            },
        )
        .unwrap();
    if after_burst.is_empty() {
        after_burst = wait_for_watermark(&mut client, &mut feed, workspace_id);
    }
    assert_eq!(after_burst, [through + 1]);
    let final_page = feed
        .next_page(&mut client, 11, None, |_| {})
        .unwrap()
        .unwrap();
    assert_eq!(final_page.len(), 1);
    assert_eq!(final_page[0].task_id, next);
    assert_eq!(feed.cursor(), through + 1);
    let (mut reader, writer) = client.into_parts();
    drop(writer);
    io::copy(&mut reader, &mut io::sink()).unwrap();
    assert!(server.0.wait().unwrap().success());
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn task_watermark_batch_is_positional_and_only_matching_commits_advance_it() {
    use fliuno_protocol::{MAX_TASK_WATERMARKS, TaskWatermarks, TaskWatermarksParams};

    let root =
        std::env::temp_dir().join(format!("fliuno-task-watermarks-{}", uuid::Uuid::new_v4()));
    fs::create_dir(&root).unwrap();
    let db = root.join("kernel.db");
    let mut peer = Peer::start(&db);
    let ws = peer.call("workspace/open", json!({"root":root}));
    let session = peer.call("session/create", json!({"workspaceId":ws["id"]}));
    let start = |peer: &mut Peer| {
        let task = peer.call(
            "task/startSummary",
            json!({"sessionId":session["id"],"input":"build an artifact"}),
        );
        let id = task["id"].as_str().unwrap().to_owned();
        peer.wait_for_status(&id, "waiting_approval");
        id
    };
    let first = start(&mut peer);
    let second = start(&mut peer);
    let request = TaskWatermarksParams {
        task_ids: vec![
            second.clone(),
            "not-found".into(),
            first.clone(),
            second.clone(),
        ],
    };
    let params = serde_json::to_value(&request).unwrap();
    let read = |peer: &mut Peer| -> TaskWatermarks {
        serde_json::from_value(peer.call("events/getTaskWatermarks", params.clone())).unwrap()
    };
    let original = read(&mut peer).through_sequences;
    assert_eq!(original[0], original[3]);
    assert_eq!(original[1], None);
    assert!(original[0].unwrap() > original[2].unwrap());
    assert_eq!(
        peer.call("events/get", json!({"sequence":original[2]}))["taskId"],
        first
    );
    let watermark_before = peer.call("events/getWatermark", json!({}));
    for invalid in [
        json!({"taskIds":[]}),
        json!({"taskIds":vec!["id"; MAX_TASK_WATERMARKS+1]}),
        json!({"taskIds":["bad id"]}),
    ] {
        assert_eq!(
            peer.request("events/getTaskWatermarks", invalid)["error"]["code"],
            -32602
        );
    }
    assert_eq!(
        peer.call("events/getWatermark", json!({})),
        watermark_before
    );
    peer.call("task/cancelSummary", json!({"taskId":first}));
    let changed = read(&mut peer).through_sequences;
    assert_eq!((&changed[..2], changed[3]), (&original[..2], original[3]));
    assert!(changed[2].unwrap() > original[2].unwrap());
    assert_eq!(
        peer.call("events/get", json!({"sequence":changed[2]}))["data"]["type"],
        "task_cancelled"
    );
    drop(peer);

    let (server, mut client) = super::ag_ui_run::connect(&db);
    assert_eq!(client.info().capabilities["taskEventWatermarks"], true);
    assert_eq!(
        client
            .task_event_watermarks(&request, |_| {})
            .unwrap()
            .through_sequences,
        changed
    );
    drop(client);
    drop(server);
    fs::remove_dir_all(root).unwrap();
}
