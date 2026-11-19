use super::*;
use fliuno_protocol::{EventsPage, WorkSnapshotParams};
use std::{cell::RefCell, io, rc::Rc};

// Submit one notification batch in front of a normal RPC request. The batch
// fills the server's live queue while still using the same stdio connection.
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

#[test]
fn work_feed_repairs_real_server_overflow_in_the_same_scope() {
    let root = std::env::temp_dir().join(format!("fliuno-rust-feed-{}", uuid::Uuid::new_v4()));
    fs::create_dir(&root).unwrap();
    let mut server = Server(
        Command::new(env!("CARGO_BIN_EXE_fliuno-app-server"))
            .arg("--db")
            .arg(root.join("kernel.db"))
            .env("FLIUNO_PROVIDER", "demo")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let queued = Rc::new(RefCell::new(None));
    let writer = BatchWriter {
        stdin: server.0.stdin.take().unwrap(),
        queued: Rc::clone(&queued),
    };
    let reader = BufReader::new(server.0.stdout.take().unwrap());
    let mut client = Client::connect(reader, writer, "rust-feed-test", |_| {}).unwrap();
    let workspace: Workspace = client
        .call("workspace/open", &json!({"root":root}), |_| {})
        .unwrap();
    let session: fliuno_protocol::Session = client
        .call(
            "session/create",
            &json!({"workspaceId":workspace.id}),
            |_| {},
        )
        .unwrap();
    let foreign: fliuno_protocol::Session = client
        .call(
            "session/create",
            &json!({"workspaceId":workspace.id}),
            |_| {},
        )
        .unwrap();
    let mut feed = client
        .subscribe_work(
            &WorkSnapshotParams {
                workspace_id: None,
                session_id: Some(session.id.clone()),
                approval_limit: 10,
                approval_target_bytes: None,
            },
            |_| {},
        )
        .unwrap()
        .events;
    assert_eq!(feed.cursor(), 0);
    assert!(
        feed.next_page(&mut client, 10, None, |_| {})
            .unwrap()
            .is_none()
    );

    let mut batch: Vec<_> = (0..72).map(|i| json!({"jsonrpc":"2.0","method":"task/start",
        "params":{"sessionId":session.id,"input":format!("task {i}"),"context":{"mode":"isolated"}}})).collect();
    batch.push(json!({"jsonrpc":"2.0","method":"task/start",
        "params":{"sessionId":foreign.id,"input":"excluded","context":{"mode":"isolated"}}}));
    let mut frame = serde_json::to_vec(&batch).unwrap();
    frame.push(b'\n');
    *queued.borrow_mut() = Some(frame);
    let mut gap = false;
    let mut observed_watermarks = 0;
    let deadline = Instant::now() + Duration::from_secs(5);
    while !gap {
        let _: Value = client
            .call(
                "workspace/get",
                &json!({"workspaceId":workspace.id}),
                |note| {
                    assert_ne!(note.method, "notifications/event");
                    if note.method == "notifications/eventWatermark" {
                        assert_eq!(note.params.as_object().unwrap().len(), 1);
                        assert!(note.params["throughSequence"].is_u64());
                        observed_watermarks += 1;
                    }
                    gap |= note.method == "notifications/eventGap";
                    assert!(feed.observe(note).is_none());
                },
            )
            .unwrap();
        if !gap {
            assert!(Instant::now() < deadline, "missing event overflow notice");
            thread::sleep(Duration::from_millis(10));
        }
    }
    assert_eq!(observed_watermarks, 64);
    let mut delivered = Vec::new();
    while let Some(page) = feed.next_page(&mut client, 11, Some(4096), |_| {}).unwrap() {
        assert!(!page.is_empty() && page.len() <= 11);
        delivered.extend(page);
    }
    let through = feed.cursor();
    let exact: EventsPage = client
        .call(
            "events/listSession",
            &json!({
                "sessionId":session.id,"after":0,"through":through,"limit":1000,
            }),
            |_| {},
        )
        .unwrap();
    assert_eq!(
        serde_json::to_value(&delivered).unwrap(),
        serde_json::to_value(exact.events).unwrap()
    );
    assert_eq!(
        delivered
            .iter()
            .filter(|event| matches!(event.data, EventKind::TaskStarted))
            .count(),
        72
    );
    assert!(
        delivered
            .windows(2)
            .all(|events| events[0].sequence < events[1].sequence)
    );
    let (mut reader, writer) = client.into_parts();
    drop(writer);
    io::copy(&mut reader, &mut io::sink()).unwrap();
    assert!(server.0.wait().unwrap().success());
    fs::remove_dir_all(root).unwrap();
}
