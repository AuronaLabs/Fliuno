//! Verify the HTTP boundary against a real stdio server and SQLite database.
use super::{ag_ui_run::connect_with_writer, *};
use fliuno_protocol::Session;
use std::{
    io,
    sync::{Arc, Mutex as SyncMutex},
};
use tokio::sync::Mutex;

struct CountWatermarks {
    stdin: ChildStdin,
    times: Arc<SyncMutex<Vec<Instant>>>,
}

impl Write for CountWatermarks {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes
            .windows(b"events/getWatermark".len())
            .any(|part| part == b"events/getWatermark")
        {
            self.times.lock().unwrap().push(Instant::now());
        }
        self.stdin.write_all(bytes)?;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.stdin.flush()
    }
}

#[tokio::test]
async fn ag_ui_http_authorizes_starts_concurrent_runs_and_resumes_sse() {
    let root = std::env::temp_dir().join(format!("fliuno-ag-ui-http-{}", uuid::Uuid::new_v4()));
    fs::create_dir(&root).unwrap();
    let watermarks = Arc::new(SyncMutex::new(Vec::new()));
    let (server, mut client) =
        connect_with_writer(&root.join("kernel.db"), |stdin| CountWatermarks {
            stdin,
            times: watermarks.clone(),
        });
    let workspace: Workspace = client
        .call("workspace/open", &json!({"root":root}), |_| {})
        .unwrap();
    let session: Session = client
        .call(
            "session/create",
            &json!({"workspaceId":workspace.id}),
            |_| {},
        )
        .unwrap();
    let other: Session = client
        .call(
            "session/create",
            &json!({"workspaceId":workspace.id}),
            |_| {},
        )
        .unwrap();
    let session_id = session.id.clone();
    let other_id = other.id.clone();
    let shared = Arc::new(Mutex::new(client));
    let router = fliuno_ag_ui_http::router(shared.clone(), move |headers| {
        match headers.get("authorization")?.to_str().ok()? {
            "Bearer local-test" => Some(session_id.clone()),
            "Bearer other-test" => Some(other_id.clone()),
            _ => None,
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let listener = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(12))
        .build()
        .unwrap();
    let new_run = |run: &str| json!({"threadId":session.id,"runId":run,"messages":[{"id":"u","role":"user","content":"prepare an artifact"}]});
    let first = new_run("http-run-1");
    assert_eq!(
        http.post(format!("{base}/run"))
            .json(&first)
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
    assert_eq!(
        http.post(format!("{base}/run"))
            .bearer_auth("other-test")
            .json(&first)
            .send()
            .await
            .unwrap()
            .status(),
        404
    );
    assert!(matches!(
        shared
            .lock()
            .await
            .attach_ag_ui_run(&session.id, "http-run-1", |_| {}),
        Err(fliuno_client::ag_ui::AgUiRunError::Rpc(Error::Rpc {
            code: -32004,
            ..
        }))
    ));
    let mut first = http
        .post(format!("{base}/run"))
        .bearer_auth("local-test")
        .json(&first)
        .send()
        .await
        .unwrap();
    assert_eq!(first.status(), 200);
    assert_eq!(first.headers()["content-type"], "text/event-stream");
    let second = http
        .post(format!("{base}/run"))
        .bearer_auth("local-test")
        .json(&new_run("http-run-2"))
        .send()
        .await
        .unwrap();
    assert_eq!(second.status(), 200);
    assert_eq!(
        http.get(format!("{base}/runs/http-run-1"))
            .bearer_auth("other-test")
            .send()
            .await
            .unwrap()
            .status(),
        404
    );
    let mut start = Vec::new();
    while !start.windows(2).any(|bytes| bytes == b"\n\n") {
        start.extend_from_slice(&first.chunk().await.unwrap().unwrap());
    }
    assert!(std::str::from_utf8(&start).unwrap().contains("RUN_STARTED"));

    // The two startup requests query their snapshots separately. After both
    // are initialized, keep the streams idle and observe actual stdio traffic.
    let idle_since = Instant::now();
    let deadline = idle_since + Duration::from_secs(4);
    loop {
        let samples = watermarks
            .lock()
            .unwrap()
            .iter()
            .copied()
            .filter(|instant| *instant >= idle_since)
            .collect::<Vec<_>>();
        if samples.len() >= 4 {
            assert!(
                samples
                    .windows(2)
                    .all(|pair| pair[1] - pair[0] >= Duration::from_millis(145)),
                "idle responses sent separate watermark queries: {:?}",
                samples
                    .windows(2)
                    .map(|pair| pair[1] - pair[0])
                    .collect::<Vec<_>>()
            );
            break;
        }
        assert!(
            Instant::now() < deadline,
            "idle listeners stopped querying the watermark"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // Both HTTP applications wait on the normal kernel approval gate. The host
    // approves through the shared client while the HTTP response remains open.
    for run_id in ["http-run-1", "http-run-2"] {
        let deadline = Instant::now() + Duration::from_secs(8);
        loop {
            {
                let mut client = shared.lock().await;
                let run = client
                    .attach_ag_ui_run(&session.id, run_id, |_| {})
                    .unwrap();
                let task: Task = client
                    .call(
                        "task/get",
                        &TaskIdParams {
                            task_id: run.task().id.clone(),
                        },
                        |_| {},
                    )
                    .unwrap();
                if let Some(approval) = task.pending_approval {
                    let _: Task = client
                        .call(
                            "approval/respond",
                            &ApprovalRespondParams {
                                task_id: task.id,
                                approval_id: approval.id,
                                decision: ApprovalDecision::AllowOnce,
                            },
                            |_| {},
                        )
                        .unwrap();
                    break;
                }
            }
            assert!(
                Instant::now() < deadline,
                "missing pending approval for {run_id}"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
    start.extend_from_slice(&first.bytes().await.unwrap());
    let output = String::from_utf8(start).unwrap();
    let second = second.text().await.unwrap();
    assert!(output.contains(r#""runId":"http-run-1""#));
    assert!(!output.contains(r#""runId":"http-run-2""#));
    assert!(second.contains(r#""runId":"http-run-2""#));
    assert!(!second.contains(r#""runId":"http-run-1""#));
    let frames: Vec<_> = output.split_terminator("\n\n").collect();
    let [content, end, finished] = &frames[frames.len() - 3..] else {
        unreachable!()
    };
    assert!(content.contains("TEXT_MESSAGE_CONTENT"));
    assert!(end.contains("TEXT_MESSAGE_END"));
    assert!(finished.contains("RUN_FINISHED"));
    let last = content
        .lines()
        .next()
        .unwrap()
        .strip_prefix("id: ")
        .unwrap();
    let invalid = format!("{}:999", last.rsplit_once(':').unwrap().0);
    assert_eq!(
        http.get(format!("{base}/runs/http-run-1"))
            .bearer_auth("local-test")
            .header("last-event-id", invalid)
            .send()
            .await
            .unwrap()
            .status(),
        400
    );
    let resumed = http
        .get(format!("{base}/runs/http-run-1"))
        .bearer_auth("local-test")
        .header("last-event-id", last)
        .send()
        .await
        .unwrap();
    assert_eq!(resumed.status(), 200);
    assert_eq!(
        resumed.text().await.unwrap(),
        format!("{end}\n\n{finished}\n\n")
    );
    listener.abort();
    let _ = listener.await;
    drop(shared);
    drop(server);
    fs::remove_dir_all(root).unwrap();
}
