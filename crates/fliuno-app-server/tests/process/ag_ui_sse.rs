//! Exercise replay-to-live SSE and an interrupted consumer across app-server restart.
use super::*;
use fliuno_client::ag_ui::AgUiSseStream;
use fliuno_protocol::{Event, EventKind, EventsPage, TaskSummary};

fn write_history(
    peer: &mut Peer,
    stream: &mut AgUiSseStream,
    task: &str,
    through: u64,
    output: &mut Vec<u8>,
) {
    let mut after = 0;
    loop {
        let page: EventsPage = serde_json::from_value(peer.call(
            "events/list",
            json!({"taskId":task,"after":after,"through":through,"limit":1}),
        ))
        .unwrap();
        for event in page.events {
            stream.write_event(event, output).unwrap();
        }
        if !page.has_more {
            break;
        }
        assert!(page.next_after > after);
        after = page.next_after;
    }
    stream.finish_replay().unwrap();
}

#[test]
fn sse_hands_replay_to_live_and_recovers_two_remaining_completion_frames() {
    let root = std::env::temp_dir().join(format!("fliuno-ag-ui-sse-{}", uuid::Uuid::new_v4()));
    fs::create_dir(&root).unwrap();
    let db = root.join("kernel.db");
    let mut peer = Peer::start(&db);
    let workspace = peer.call("workspace/open", json!({"root":root}));
    let session = peer.call("session/create", json!({"workspaceId":workspace["id"]}));
    let task = peer.call(
        "task/start",
        json!({"sessionId":session["id"],"input":"交付物"}),
    );
    let id = task["id"].as_str().unwrap();
    let waiting = peer.wait_for_status(id, "waiting_approval");
    let summary: TaskSummary =
        serde_json::from_value(peer.call("task/getSummary", json!({"taskId":id}))).unwrap();

    let mut stream = AgUiSseStream::new(&summary, None).unwrap();
    let mut bytes = Vec::new();
    let subscription = peer.call("events/setScope", json!({"taskId":id,"after":0}));
    let through = subscription["throughSequence"].as_u64().unwrap();
    write_history(&mut peer, &mut stream, id, through, &mut bytes);
    assert!(String::from_utf8_lossy(&bytes).contains("fliuno.approval_requested"));

    peer.notifications.clear();
    peer.call(
        "approval/respond",
        json!({"taskId":id,"approvalId":waiting["pendingApproval"]["id"],"decision":"allow_once"}),
    );
    peer.wait_for_status(id, "completed");
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut notifications = std::mem::take(&mut peer.notifications).into_iter();
    let completion_sequence = loop {
        let note = notifications.next().unwrap_or_else(|| {
            peer.output
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .unwrap()
        });
        assert_ne!(note["method"], "notifications/eventGap");
        if note["method"] != "notifications/event" {
            continue;
        }
        let event: Event = serde_json::from_value(note["params"]["event"].clone()).unwrap();
        if event.sequence <= through {
            continue;
        }
        let finished = matches!(event.data, EventKind::TaskCompleted { .. });
        let sequence = event.sequence;
        stream.write_event(event, &mut bytes).unwrap();
        if finished {
            break sequence;
        }
    };
    drop(peer);

    let last_id = format!("{id}:{completion_sequence}:2");
    let remaining_id = format!("id: {id}:{completion_sequence}:3\n");
    let original = std::str::from_utf8(&bytes).unwrap();
    assert!(original.contains(&format!("id: {last_id}\ndata:")));
    let expected = &original[original.find(&remaining_id).unwrap()..];
    let mut peer = Peer::start(&db);
    let mut resumed = AgUiSseStream::new(&summary, Some(&last_id)).unwrap();
    let mut resumed_bytes = Vec::new();
    let subscription = peer.call("events/setScope", json!({"taskId":id,"after":0}));
    write_history(
        &mut peer,
        &mut resumed,
        id,
        subscription["throughSequence"].as_u64().unwrap(),
        &mut resumed_bytes,
    );
    assert_eq!(resumed_bytes, expected.as_bytes());
    let frames = expected.strip_suffix("\n\n").unwrap().split("\n\n");
    let payloads: Vec<Value> = frames
        .map(|frame| {
            let (_, data) = frame.split_once("\ndata: ").unwrap();
            serde_json::from_str(data).unwrap()
        })
        .collect();
    assert_eq!(payloads.len(), 2);
    assert_eq!(payloads[0]["type"], "TEXT_MESSAGE_END");
    assert_eq!(payloads[1]["type"], "RUN_FINISHED");
    assert_eq!(payloads[1]["threadId"], session["id"]);
    drop(peer);
    fs::remove_dir_all(root).unwrap();
}
