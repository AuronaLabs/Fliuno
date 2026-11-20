//! Verify the HTTP boundary against a real stdio server and SQLite database.
use super::{ag_ui_run::connect_with_writer, *};
use fliuno_protocol::Session;
use std::{
    io,
    sync::{Arc, Mutex as SyncMutex},
};
use tokio::sync::Mutex;

#[derive(Default)]
struct Traffic {
    scoped: Vec<(Instant, Vec<String>)>,
    global: Vec<Instant>,
    replay: Vec<(Instant, String)>,
}

struct CountWatermarks {
    stdin: ChildStdin,
    traffic: Arc<SyncMutex<Traffic>>,
}

impl Write for CountWatermarks {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let request: Value = serde_json::from_slice(bytes).unwrap();
        match request["method"].as_str().unwrap() {
            "events/getWatermark" => self.traffic.lock().unwrap().global.push(Instant::now()),
            "events/getTaskWatermarks" => {
                let mut ids: Vec<String> =
                    serde_json::from_value(request["params"]["taskIds"].clone()).unwrap();
                ids.sort_unstable();
                self.traffic
                    .lock()
                    .unwrap()
                    .scoped
                    .push((Instant::now(), ids));
            }
            "events/list" => self.traffic.lock().unwrap().replay.push((
                Instant::now(),
                request["params"]["taskId"].as_str().unwrap().to_owned(),
            )),
            _ => {}
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
    let watermarks = Arc::new(SyncMutex::new(Traffic::default()));
    let (server, mut client) =
        connect_with_writer(&root.join("kernel.db"), |stdin| CountWatermarks {
            stdin,
            traffic: watermarks.clone(),
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
    let reviewer_session = session.id.clone();
    let other_reviewer_session = other.id.clone();
    let router = fliuno_ag_ui_http::router_with_approvals(
        shared.clone(),
        move |headers| match headers.get("authorization")?.to_str().ok()? {
            "Bearer local-test" => Some(session_id.clone()),
            "Bearer other-test" => Some(other_id.clone()),
            _ => None,
        },
        move |headers| match headers.get("authorization")?.to_str().ok()? {
            "Bearer reviewer-test" => Some(reviewer_session.clone()),
            "Bearer other-reviewer-test" => Some(other_reviewer_session.clone()),
            _ => None,
        },
    );
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

    // Three bodies watch two tasks. Verify duplicate reconnects share the
    // same task sample and that foreign commits cannot trigger empty replays.
    let duplicate = http
        .get(format!("{base}/runs/http-run-1"))
        .bearer_auth("local-test")
        .send()
        .await
        .unwrap();
    assert_eq!(duplicate.status(), 200);
    let mut watched = {
        let mut client = shared.lock().await;
        ["http-run-1", "http-run-2"].map(|id| {
            client
                .attach_ag_ui_run(&session.id, id, |_| {})
                .unwrap()
                .task()
                .id
                .clone()
        })
    };
    watched.sort_unstable();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let mut client = shared.lock().await;
        if watched.iter().all(|id| {
            let summary: TaskSummary = client
                .call("task/getSummary", &json!({"taskId":id}), |_| {})
                .unwrap();
            summary.status == TaskStatus::WaitingApproval
        }) {
            break;
        }
        drop(client);
        assert!(Instant::now() < deadline, "runs never reached approval");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let settled = Instant::now();
    let deadline = settled + Duration::from_secs(5);
    loop {
        if watermarks
            .lock()
            .unwrap()
            .scoped
            .iter()
            .filter(|(at, _)| *at >= settled)
            .count()
            >= 3
        {
            break;
        }
        assert!(Instant::now() < deadline, "scoped polling did not start");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let idle_since = Instant::now();
    let deadline = idle_since + Duration::from_secs(5);
    for expected in 1..=4 {
        {
            let mut client = shared.lock().await;
            let foreign: TaskSummary = client
                .call(
                    "task/startSummary",
                    &json!({"sessionId":other.id,"input":"unrelated artifact"}),
                    |_| {},
                )
                .unwrap();
            let _: TaskSummary = client
                .call("task/cancelSummary", &json!({"taskId":foreign.id}), |_| {})
                .unwrap();
        }
        loop {
            if watermarks
                .lock()
                .unwrap()
                .scoped
                .iter()
                .filter(|(at, _)| *at >= idle_since)
                .count()
                >= expected
            {
                break;
            }
            assert!(Instant::now() < deadline, "idle listeners stopped polling");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
    {
        let traffic = watermarks.lock().unwrap();
        let samples = traffic
            .scoped
            .iter()
            .filter(|(at, _)| *at >= idle_since)
            .collect::<Vec<_>>();
        assert!(samples.iter().all(|(_, ids)| ids.as_slice() == watched));
        assert!(
            samples
                .windows(2)
                .all(|pair| pair[1].0 - pair[0].0 >= Duration::from_millis(145)),
            "idle bodies sent separate watermark queries"
        );
        assert!(
            !traffic.replay.iter().any(|(at, _)| *at >= idle_since),
            "foreign work woke an idle stream"
        );
        assert!(
            !traffic.global.iter().any(|at| *at >= idle_since),
            "idle streams polled the global watermark"
        );
    }
    drop(duplicate);

    let mut first_approval = None;
    for run_id in ["http-run-1", "http-run-2"] {
        let deadline = Instant::now() + Duration::from_secs(8);
        let approval_id = loop {
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
                    break approval.id;
                }
            }
            assert!(
                Instant::now() < deadline,
                "missing pending approval for {run_id}"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        };
        let url = format!("{base}/runs/{run_id}/approvals/{approval_id}");
        let decide = |token, decision| {
            http.post(&url)
                .bearer_auth(token)
                .json(&json!({"decision":decision}))
        };
        assert_eq!(
            decide("local-test", "allow_once")
                .send()
                .await
                .unwrap()
                .status(),
            401
        );
        assert_eq!(
            decide("other-reviewer-test", "allow_once")
                .send()
                .await
                .unwrap()
                .status(),
            404
        );
        if let Some(other_id) = &first_approval {
            let cross = format!("{base}/runs/{run_id}/approvals/{other_id}");
            assert_eq!(
                http.post(cross)
                    .bearer_auth("reviewer-test")
                    .json(&json!({"decision":"allow_once"}))
                    .send()
                    .await
                    .unwrap()
                    .status(),
                404
            );
        } else {
            first_approval = Some(approval_id.clone());
        }
        let view = http
            .get(&url)
            .bearer_auth("reviewer-test")
            .send()
            .await
            .unwrap();
        assert_eq!(view.status(), 200);
        let view: Value = view.json().await.unwrap();
        assert_eq!(view["kind"], "tool_call");
        assert_eq!(view["approval"]["id"], approval_id);
        let first = decide("reviewer-test", "allow_once").send().await.unwrap();
        assert_eq!(first.status(), 200);
        let first: Value = first.json().await.unwrap();
        let repeated = decide("reviewer-test", "allow_once").send().await.unwrap();
        assert_eq!(repeated.status(), 200);
        assert_eq!(repeated.json::<Value>().await.unwrap(), first);
        assert_eq!(
            decide("reviewer-test", "deny")
                .send()
                .await
                .unwrap()
                .status(),
            409
        );
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
    let finished_path = format!("{base}/runs/http-run-1");
    assert_eq!(
        http.delete(finished_path)
            .bearer_auth("local-test")
            .send()
            .await
            .unwrap()
            .status(),
        409
    );
    let cancel = http
        .post(format!("{base}/run"))
        .bearer_auth("local-test")
        .json(&new_run("http-cancel"))
        .send()
        .await
        .unwrap();
    assert_eq!(cancel.status(), 200);
    let cancel_path = format!("{base}/runs/http-cancel");
    assert_eq!(
        http.delete(&cancel_path).send().await.unwrap().status(),
        401
    );
    assert_eq!(
        http.delete(&cancel_path)
            .bearer_auth("other-test")
            .send()
            .await
            .unwrap()
            .status(),
        404
    );
    for _ in 0..2 {
        let response = http
            .delete(&cancel_path)
            .bearer_auth("local-test")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 204);
        assert!(response.bytes().await.unwrap().is_empty());
    }
    let cancelled = cancel.text().await.unwrap();
    let frame = cancelled.split_terminator("\n\n").last().unwrap();
    let event: Value = serde_json::from_str(frame.split_once("\ndata: ").unwrap().1).unwrap();
    assert_eq!(
        event,
        json!({"type":"RUN_FINISHED","threadId":session.id,"runId":"http-cancel","outcome":{"type":"cancelled"}})
    );
    assert_eq!(cancelled.matches("RUN_FINISHED").count(), 1);

    listener.abort();
    let _ = listener.await;
    drop(shared);
    drop(server);
    fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn ag_ui_http_inline_media_is_authorized_and_durable() {
    const PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+yhE0AAAAASUVORK5CYII=";
    const PDF: &str = "JVBERi0xLjQKMSAwIG9iago8PCAvVHlwZSAvQ2F0YWxvZyAvUGFnZXMgMiAwIFIgPj4KZW5kb2JqCjIgMCBvYmoKPDwgL1R5cGUgL1BhZ2VzIC9LaWRzIFszIDAgUl0gL0NvdW50IDEgPj4KZW5kb2JqCjMgMCBvYmoKPDwgL1R5cGUgL1BhZ2UgL1BhcmVudCAyIDAgUiAvTWVkaWFCb3ggWzAgMCAxMCAxMF0gPj4KZW5kb2JqCnhyZWYKMCA0CjAwMDAwMDAwMDAgNjU1MzUgZiAKMDAwMDAwMDAwOSAwMDAwMCBuIAowMDAwMDAwMDU4IDAwMDAwIG4gCjAwMDAwMDAxMTUgMDAwMDAgbiAKdHJhaWxlcgo8PCAvU2l6ZSA0IC9Sb290IDEgMCBSID4+CnN0YXJ0eHJlZgoxODQKJSVFT0YK";
    let root = std::env::temp_dir().join(format!("fliuno-ag-ui-image-{}", uuid::Uuid::new_v4()));
    fs::create_dir(&root).unwrap();
    let db = root.join("kernel.db");
    let (server, mut client) = super::ag_ui_run::connect(&db);
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
    let session_id = session.id.clone();
    let shared = Arc::new(Mutex::new(client));
    let router = fliuno_ag_ui_http::router_with_interrupts(
        shared.clone(),
        move |headers| {
            (headers.get("authorization")?.to_str().ok()? == "Bearer image-test")
                .then(|| session_id.clone())
        },
        |_| None,
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/run", listener.local_addr().unwrap());
    let listener = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(12))
        .build()
        .unwrap();
    let run = json!({"threadId":session.id,"runId":"image-run","messages":[
        {"id":"u","role":"user","content":[
            {"type":"text","text":"Before "},
            {"type":"image","source":{"type":"data","mimeType":"image/png","value":PNG}},
            {"type":"text","text":" after "},
            {"type":"document","source":{"type":"data","mimeType":"application/pdf","value":PDF}}
        ]}
    ]});
    assert_eq!(
        http.post(&endpoint)
            .json(&run)
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
    for rejected_source in [
        json!({"type":"url","value":"http://127.0.0.1/image.png"}),
        json!({"type":"file","value":"file-1","provider":"openai"}),
        json!({"type":"data","mimeType":"image/png","value":"not base64"}),
        json!({"type":"data","mimeType":"image/jpeg","value":PNG}),
    ] {
        let mut rejected = run.clone();
        rejected["messages"][0]["content"][1]["source"] = rejected_source;
        let response = http
            .post(&endpoint)
            .bearer_auth("image-test")
            .json(&rejected)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 400);
    }
    for source in [
        json!({"type":"data","mimeType":"image/png","value":PDF}),
        json!({"type":"data","mimeType":"application/pdf","value":PNG}),
    ] {
        let mut rejected = run.clone();
        rejected["messages"][0]["content"][3]["source"] = source;
        let response = http
            .post(&endpoint)
            .bearer_auth("image-test")
            .json(&rejected)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 400);
    }
    let response = http
        .post(&endpoint)
        .bearer_auth("image-test")
        .json(&run)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let sse = response.text().await.unwrap();
    assert!(sse.contains("RUN_FINISHED"), "{sse}");
    assert!(!sse.contains(PNG));
    assert!(!sse.contains(PDF));
    let task_id = shared
        .lock()
        .await
        .attach_ag_ui_interrupt_run(&session.id, "image-run", |_| {})
        .unwrap()
        .task()
        .id
        .clone();
    listener.abort();
    assert!(listener.await.unwrap_err().is_cancelled());
    drop(shared);
    drop(server);

    let (server, mut client) = super::ag_ui_run::connect(&db);
    let parsed =
        fliuno_client::ag_ui::AgUiRunInput::parse(&serde_json::to_vec(&run).unwrap()).unwrap();
    let repeated = client
        .start_ag_ui_interrupt_run(&session.id, parsed, |_| {})
        .unwrap();
    assert_eq!(repeated.task().id, task_id);
    assert_eq!(
        repeated.task().input_preview,
        "Before [image] after [document]"
    );
    let task: Task = client
        .call("task/get", &json!({"taskId":task_id}), |_| {})
        .unwrap();
    let fliuno_protocol::Message::UserParts { parts } = &task.messages[0] else {
        panic!("stored input is not multipart");
    };
    assert!(
        matches!(&parts[1], fliuno_protocol::UserContentPart::Image { data, .. } if data == PNG)
    );
    assert!(matches!(&parts[3], fliuno_protocol::UserContentPart::Pdf { data } if data == PDF));
    let mut changed = run;
    changed["messages"][0]["content"][1]["source"]["value"] = json!("iVBORw0KGgo=");
    let parsed =
        fliuno_client::ag_ui::AgUiRunInput::parse(&serde_json::to_vec(&changed).unwrap()).unwrap();
    assert!(matches!(
        client.start_ag_ui_interrupt_run(&session.id, parsed, |_| {}),
        Err(fliuno_client::ag_ui::AgUiRunError::Rpc(Error::Rpc {
            code: -32009,
            ..
        }))
    ));
    let _: Task = client
        .call(
            "approval/respond",
            &ApprovalRespondParams {
                task_id: task_id.clone(),
                approval_id: task.pending_approval.unwrap().id,
                decision: ApprovalDecision::AllowOnce,
            },
            |_| {},
        )
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    let history = loop {
        let history: Value = client
            .call(
                "session/listHistory",
                &json!({"sessionId":session.id}),
                |_| {},
            )
            .unwrap();
        if history
            .to_string()
            .contains("Before [image] after [document]")
        {
            break history;
        }
        assert!(
            Instant::now() < deadline,
            "completed task missing from history"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    };
    assert!(!history.to_string().contains(PNG));
    assert!(!history.to_string().contains(PDF));
    drop(client);
    drop(server);
    fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn ag_ui_http_reuses_artifacts_only_in_opted_in_sessions_and_root_runs() {
    use axum::http::{HeaderMap, StatusCode};
    use fliuno_ag_ui_http::RouterBuilder;
    use fliuno_protocol::{Message, UserContentPart};
    use http_wire::{Fixture, Response};

    const PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+yhE0AAAAASUVORK5CYII=";
    let root = std::env::temp_dir().join(format!("fliuno-ag-ui-artifact-{}", uuid::Uuid::new_v4()));
    fs::create_dir(&root).unwrap();
    let db = root.join("kernel.db");
    let fixture = Fixture::start(|request| {
        let output = if request.body["input"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["type"] == "function_call_output")
        {
            json!([{"type":"message","content":[{"type":"output_text","text":"saved"}]}])
        } else {
            let arguments = json!({"title":"chart","mediaType":"image/png","contentBase64":PNG});
            json!([{"type":"function_call","call_id":"chart","name":"artifact_create_binary","arguments":arguments.to_string()}])
        };
        Response::json(200, json!({"status":"completed","output":output}))
    });
    let (session, other, artifact) =
        {
            let mut peer = Peer::start_with_model_fixture(&db, Some(&fixture.url("/v1/")));
            let workspace = peer.call("workspace/open", json!({"root":root}));
            let mut create_session = || {
                peer.call("session/create", json!({"workspaceId":workspace["id"]}))["id"]
                    .as_str()
                    .unwrap()
                    .to_owned()
            };
            let session = create_session();
            let other = create_session();
            let source = peer.call(
                "task/start",
                json!({"sessionId":session,"input":"save chart"}),
            );
            let id = source["id"].as_str().unwrap();
            let waiting = peer.wait_for_status(id, "waiting_approval");
            peer.call("approval/respond", json!({
            "taskId":id,"approvalId":waiting["pendingApproval"]["id"],"decision":"allow_once"
        }));
            peer.wait_for_status(id, "completed");
            let manifest = peer.call("artifacts/listSummaries", json!({"taskId":id}));
            (
                session,
                other,
                json!({"taskId":id,"artifactId":manifest["items"][0]["id"]}),
            )
        };
    let (server, client) = super::ag_ui_run::connect(&db);
    let client = Arc::new(Mutex::new(client));
    let submitter = {
        let session = session.clone();
        let other = other.clone();
        move |headers: &HeaderMap| match headers.get("authorization")?.to_str().ok()? {
            "Bearer runner" => Some(session.clone()),
            "Bearer other" => Some(other.clone()),
            _ => None,
        }
    };
    let reviewer = {
        let session = session.clone();
        move |headers: &HeaderMap| {
            (headers.get("authorization")? == "Bearer reviewer").then(|| session.clone())
        }
    };
    let enabled = RouterBuilder::new(client.clone(), submitter.clone())
        .interrupts(reviewer)
        .artifact_inputs()
        .build();
    let disabled = RouterBuilder::new(client.clone(), submitter).build();
    let routes = axum::Router::new()
        .nest("/on", enabled)
        .nest("/off", disabled);
    let socket = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", socket.local_addr().unwrap());
    let listener = tokio::spawn(async move { axum::serve(socket, routes).await.unwrap() });
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(12))
        .build()
        .unwrap();
    let send = |route, token, body: &Value| {
        http.post(format!("{base}/{route}/run"))
            .bearer_auth(token)
            .json(body)
            .send()
    };
    let props = json!({"fliuno":{"artifactInputs":[artifact]}});
    let run = json!({
        "threadId":session,"runId":"review-chart",
        "messages":[{"id":"u","role":"user","content":""}],"forwardedProps":props
    });
    let mut outside = run.clone();
    outside["threadId"] = json!(other);
    for (route, token, input, status) in [
        ("off", "runner", &run, StatusCode::BAD_REQUEST),
        ("on", "reviewer", &run, StatusCode::UNAUTHORIZED),
        ("on", "other", &outside, StatusCode::NOT_FOUND),
    ] {
        assert_eq!(send(route, token, input).await.unwrap().status(), status);
    }
    let response = send("on", "runner", &run).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let sse = response.text().await.unwrap();
    assert!(sse.contains("approval_required"), "{sse}");
    assert!(!sse.contains(PNG));
    let task: Task = {
        let mut locked = client.lock().await;
        let run = locked
            .attach_ag_ui_interrupt_run(&session, "review-chart", |_| {})
            .unwrap();
        locked
            .call("task/get", &json!({"taskId":run.task().id}), |_| {})
            .unwrap()
    };
    assert!(matches!(&task.messages[0], Message::UserParts { parts } if
        matches!(parts.as_slice(), [UserContentPart::Image { data, .. }] if data == PNG)));
    let review = json!({
        "threadId":session,"runId":"review-child","parentRunId":"review-chart","messages":[],
        "resume":[{"interruptId":task.pending_approval.unwrap().id,"status":"resolved","payload":{"decision":"allow_once"}}]
    });
    let mut invalid_review = review.clone();
    invalid_review["forwardedProps"] = props;
    assert_eq!(
        send("on", "reviewer", &invalid_review)
            .await
            .unwrap()
            .status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        send("on", "runner", &review).await.unwrap().status(),
        StatusCode::UNAUTHORIZED
    );
    let response = send("on", "reviewer", &review).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(response.text().await.unwrap().contains("RUN_FINISHED"));
    let response = send("on", "runner", &run).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.text().await.unwrap(), sse);
    listener.abort();
    assert!(listener.await.unwrap_err().is_cancelled());
    drop(client);
    drop(server);
    fs::remove_dir_all(root).unwrap();
}
