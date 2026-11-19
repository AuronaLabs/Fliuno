//! Exercise standard interrupts over real HTTP and a restartable stdio peer.
use super::{ag_ui_run::connect, *};
use fliuno_protocol::Session;
use reqwest::{Client as Http, Response, StatusCode};
use std::sync::Arc;
use tokio::{net::TcpListener, sync::Mutex, task::JoinHandle};

async fn listen(router: axum::Router) -> (JoinHandle<()>, String) {
    let socket = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", socket.local_addr().unwrap());
    (
        tokio::spawn(async move { axum::serve(socket, router).await.unwrap() }),
        base,
    )
}

fn frames(sse: &str) -> Vec<(&str, Value)> {
    sse.split_terminator("\n\n")
        .filter(|frame| !frame.starts_with(':'))
        .map(|frame| {
            let (id, data) = frame
                .strip_prefix("id: ")
                .unwrap()
                .split_once("\ndata: ")
                .unwrap();
            (id, serde_json::from_str(data).unwrap())
        })
        .collect()
}

async fn accepted(response: Response) -> String {
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["content-type"], "text/event-stream");
    response.text().await.unwrap()
}

#[tokio::test]
async fn ag_ui_http_interrupts_resume_with_a_new_run_and_replay_after_restart() {
    let root = std::env::temp_dir().join(format!("fliuno-http-interrupt-{}", uuid::Uuid::new_v4()));
    fs::create_dir(&root).unwrap();
    let db = root.join("kernel.db");
    let (server, mut client) = connect(&db);
    let workspace: Workspace = client
        .call("workspace/open", &json!({"root":root}), |_| {})
        .unwrap();
    let mut make_session = || {
        client
            .call::<_, Session>(
                "session/create",
                &json!({"workspaceId":workspace.id}),
                |_| {},
            )
            .unwrap()
            .id
    };
    let session = make_session();
    let other = make_session();
    let submitter = {
        let session = session.clone();
        move |headers: &axum::http::HeaderMap| {
            (headers.get("authorization")? == "Bearer runner").then(|| session.clone())
        }
    };
    let reviewer = {
        let session = session.clone();
        move |headers: &axum::http::HeaderMap| match headers.get("authorization")?.to_str().ok()? {
            "Bearer reviewer" => Some(session.clone()),
            "Bearer other" => Some(other.clone()),
            _ => None,
        }
    };
    let client = Arc::new(Mutex::new(client));
    let (listener, base) = listen(fliuno_ag_ui_http::router_with_interrupts(
        client.clone(),
        submitter.clone(),
        reviewer.clone(),
    ))
    .await;
    let (legacy_listener, legacy) = listen(fliuno_ag_ui_http::router_with_approvals(
        client.clone(),
        submitter.clone(),
        reviewer.clone(),
    ))
    .await;
    let http = Http::builder()
        .timeout(Duration::from_secs(8))
        .build()
        .unwrap();
    let post = |base: &str, token: &str, value: &Value| {
        http.post(format!("{base}/run"))
            .bearer_auth(token)
            .json(value)
    };
    let get = |base: &str, token: &str, id: &str| {
        http.get(format!("{base}/runs/{id}")).bearer_auth(token)
    };
    let start = json!({"threadId":session,"runId":"parent","messages":[{"id":"u","role":"user","content":"prepare one artifact"}]});
    assert_eq!(
        post(&base, "reviewer", &start)
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
    let parent_sse = accepted(post(&base, "runner", &start).send().await.unwrap()).await;
    let parent = frames(&parent_sse);
    assert_eq!(parent.len(), 2);
    assert_eq!(
        parent[0].1,
        json!({"type":"RUN_STARTED","threadId":session,"runId":"parent"})
    );
    let interrupt = &parent[1].1["outcome"]["interrupts"][0];
    assert_eq!(parent[1].1["type"], "RUN_FINISHED");
    assert_eq!(parent[1].1["outcome"]["type"], "interrupt");
    assert_eq!(interrupt["reason"], "approval_required");
    let approval = interrupt["id"].as_str().unwrap();
    let detail = format!("parent/approvals/{approval}");
    assert_eq!(
        get(&base, "runner", &detail).send().await.unwrap().status(),
        401
    );
    let response = get(&base, "reviewer", &detail).send().await.unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(
        response.json::<Value>().await.unwrap()["approval"]["id"],
        approval
    );
    assert_eq!(
        http.post(format!("{base}/runs/{detail}"))
            .bearer_auth("reviewer")
            .json(&json!({"decision":"allow_once"}))
            .send()
            .await
            .unwrap()
            .status(),
        405
    );
    assert_eq!(
        http.post(format!("{legacy}/runs/{detail}"))
            .bearer_auth("reviewer")
            .json(&json!({"decision":"allow_once"}))
            .send()
            .await
            .unwrap()
            .status(),
        404
    );
    let resume = json!({"threadId":session,"runId":"child","parentRunId":"parent","messages":[],
        "resume":[{"interruptId":approval,"status":"resolved","payload":{"decision":"allow_once"}}]});
    assert_eq!(
        post(&base, "runner", &resume)
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
    assert_eq!(
        post(&base, "other", &resume).send().await.unwrap().status(),
        404
    );
    let mut wrong = resume.clone();
    wrong["parentRunId"] = json!("missing");
    assert_eq!(
        post(&base, "reviewer", &wrong)
            .send()
            .await
            .unwrap()
            .status(),
        404
    );

    let child_sse = accepted(post(&base, "reviewer", &resume).send().await.unwrap()).await;
    let child = frames(&child_sse);
    assert_eq!(
        child.first().unwrap().1,
        json!({"type":"RUN_STARTED","threadId":session,"runId":"child"})
    );
    assert_eq!(child.last().unwrap().1["type"], "RUN_FINISHED");
    assert_eq!(child.last().unwrap().1["runId"], "child");
    assert_eq!(
        child
            .iter()
            .filter(|(_, event)| event["name"] == "fliuno.artifact_created")
            .count(),
        1
    );
    assert_eq!(
        accepted(get(&base, "runner", "parent").send().await.unwrap()).await,
        parent_sse
    );
    assert_eq!(
        http.delete(format!("{base}/runs/parent"))
            .bearer_auth("runner")
            .send()
            .await
            .unwrap()
            .status(),
        409
    );
    for (id, cursor) in [("parent", child[0].0), ("child", parent[1].0)] {
        assert_eq!(
            get(&base, "runner", id)
                .header("last-event-id", cursor)
                .send()
                .await
                .unwrap()
                .status(),
            400
        );
    }
    let after = accepted(
        get(&base, "reviewer", "child")
            .header("last-event-id", child[0].0)
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(frames(&after), child[1..]);
    listener.abort();
    legacy_listener.abort();
    let _ = listener.await;
    let _ = legacy_listener.await;
    drop(client);
    drop(server);

    let (server, client) = connect(&db);
    let client = Arc::new(Mutex::new(client));
    let (listener, base) = listen(fliuno_ag_ui_http::router_with_interrupts(
        client.clone(),
        submitter,
        reviewer,
    ))
    .await;
    assert_eq!(
        accepted(post(&base, "reviewer", &resume).send().await.unwrap()).await,
        child_sse
    );
    let mut different = resume;
    different["resume"][0]["payload"]["decision"] = json!("deny");
    assert_eq!(
        post(&base, "reviewer", &different)
            .send()
            .await
            .unwrap()
            .status(),
        409
    );
    let task_id = client
        .lock()
        .await
        .attach_ag_ui_interrupt_run(&session, "child", |_| {})
        .unwrap()
        .task()
        .id
        .clone();
    let result: Value = client
        .lock()
        .await
        .call("events/list", &json!({"taskId":task_id}), |_| {})
        .unwrap();
    assert_eq!(
        result["events"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|event| event["data"]["type"] == "tool_started")
            .count(),
        1
    );
    listener.abort();
    let _ = listener.await;
    drop(client);
    drop(server);
    fs::remove_dir_all(root).unwrap();
}
