use super::{tests::rpc, *};
use std::{
    thread,
    time::{Duration, Instant},
};

struct PlanningModel;

impl fliuno_core::ModelProvider for PlanningModel {
    fn next(
        &self,
        task: &Task,
        tools: &[fliuno_core::ToolSpec],
        _: &fliuno_core::CancellationToken,
    ) -> Result<fliuno_core::ModelTurn, String> {
        assert!(tools.iter().any(|tool| tool.name == "task_ask_user"));
        let decision = match task.model_steps {
            1 => fliuno_core::ModelDecision::Call(ToolCall {
                name: "task_publish_plan".into(),
                id: "plan".into(),
                arguments: json!({"steps":[{"text":"Confirm the format","status":"in_progress"}]}),
            }),
            2 => fliuno_core::ModelDecision::Call(ToolCall {
                name: "task_ask_user".into(),
                id: "question".into(),
                arguments: json!({"question":"Which format?"}),
            }),
            _ => {
                let answer = task
                    .messages
                    .iter()
                    .find_map(|message| match message {
                        Message::ToolResult { call_id, output } if call_id == "question" => {
                            output["answer"].as_str()
                        }
                        _ => None,
                    })
                    .ok_or_else(|| "missing the user's answer".to_string())?;
                fliuno_core::ModelDecision::Complete(format!("Format: {answer}"))
            }
        };
        Ok(decision.into())
    }
}

#[test]
fn model_publishes_a_plan_and_receives_one_answer_after_restart() {
    let root = std::env::temp_dir().join(format!("fliuno-clarification-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&root).unwrap();
    let db = root.join("task.sqlite");
    let open = || {
        let mut server = RpcServer::new(Kernel::open(&db, Box::new(PlanningModel)).unwrap());
        rpc(
            &mut server,
            "initialize",
            json!({"protocolVersion":"0.1","clientName":"plan-test"}),
        );
        server
    };
    let wait = |server: &mut RpcServer, id: &Value, wanted: &str| {
        let deadline = Instant::now() + Duration::from_secs(4);
        loop {
            let task = rpc(server, "task/get", json!({"taskId":id}));
            let status = task["status"].as_str().unwrap();
            if status == wanted {
                return task;
            }
            assert!(
                !matches!(status, "failed" | "completed" | "cancelled" | "interrupted")
                    && Instant::now() < deadline,
                "unexpected task: {task}"
            );
            thread::sleep(Duration::from_millis(5));
        }
    };
    let mut server = open();
    let workspace = rpc(&mut server, "workspace/open", json!({"root":root}));
    let session = rpc(
        &mut server,
        "session/create",
        json!({"workspaceId":workspace["id"]}),
    );
    let task = rpc(
        &mut server,
        "task/start",
        json!({"sessionId":session["id"],"input":"make a report","context":{"enableInteraction":true}}),
    );
    let waiting = wait(&mut server, &task["id"], "waiting_input");
    assert_eq!(
        waiting["pendingToolInput"]["requests"]["question"],
        "Which format?"
    );
    let plan = rpc(&mut server, "task/getPlan", json!({"taskId":task["id"]}));
    assert_eq!(plan["steps"][0]["text"], "Confirm the format");
    drop(server);
    let mut server = open();
    assert_eq!(
        rpc(&mut server, "task/getPlan", json!({"taskId":task["id"]})),
        plan
    );
    let response = json!({"taskId":task["id"],"callId":"question","requestId":"response","responses":{"answer":"Markdown"}});
    rpc(&mut server, "task/respondToolInput", response.clone());
    let complete = wait(&mut server, &task["id"], "completed");
    assert_eq!(complete["output"], "Format: Markdown");
    let replay = rpc(&mut server, "task/respondToolInput", response);
    assert_eq!(replay["modelSteps"], 3);
    drop(server);
    for path in [db, root.join("task.sqlite.lock")] {
        std::fs::remove_file(path).unwrap();
    }
    std::fs::remove_dir(root).unwrap();
}

#[test]
fn detached_client_resumes_on_shared_host_and_links_reviewed_artifacts() {
    let host = RpcServer::new(Kernel::in_memory(Box::new(fliuno_core::DemoProvider)).unwrap());
    let attach = || {
        let mut rpc_server = host.attach_connection().unwrap();
        rpc(
            &mut rpc_server,
            "initialize",
            json!({"protocolVersion":"0.1","clientName":"review-test"}),
        );
        rpc_server
    };
    let mut server = attach();
    let workspace = rpc(
        &mut server,
        "workspace/open",
        json!({"root":std::env::current_dir().unwrap()}),
    );
    let session = rpc(
        &mut server,
        "session/create",
        json!({"workspaceId":workspace["id"]}),
    );
    let wait = |server: &mut RpcServer, id: &Value, wanted: &str| {
        let deadline = Instant::now() + Duration::from_secs(4);
        loop {
            let task = rpc(server, "task/get", json!({"taskId":id}));
            let status = task["status"].as_str().unwrap();
            if status == wanted {
                return task;
            }
            assert!(
                !matches!(status, "failed" | "completed" | "cancelled" | "interrupted")
                    && Instant::now() < deadline,
                "unexpected task: {task}"
            );
            thread::sleep(Duration::from_millis(5));
        }
    };
    let instruction = rpc(
        &mut server,
        "workspace/setInstructions",
        json!({"workspaceId":workspace["id"],"expectedRevision":0,"text":"Use Markdown"}),
    );
    let started = rpc(
        &mut server,
        "task/start",
        json!({"sessionId":session["id"],"input":"Draft one","context":{"instructionsRevision":instruction["revision"]}}),
    );
    let first_id = started["id"].clone();
    drop(server);
    let mut server = attach();
    let first = wait(&mut server, &first_id, "waiting_approval");
    let snapshot = rpc(
        &mut server,
        "work/subscribe",
        json!({"sessionId":session["id"],"includeActions":true}),
    );
    assert_eq!(
        snapshot["nextActions"][0]["kind"]["type"],
        "review_approval"
    );
    let attribution = rpc(
        &mut server,
        "task/contextAttribution",
        json!({"taskId":first_id}),
    );
    assert_eq!(attribution["instructionsRevision"], 1);
    assert!(attribution["instructionsBytes"].as_u64().unwrap() > 0);
    let finish = |server: &mut RpcServer, pending: Value| {
        let id = &pending["id"];
        rpc(
            server,
            "approval/respond",
            json!({"taskId":id,"approvalId":pending["pendingApproval"]["id"],"decision":"allow_once"}),
        );
        wait(server, id, "completed");
        let artifacts = rpc(server, "artifacts/list", json!({"taskId":id}));
        json!({"taskId":id,"artifactId":artifacts[0]["id"]})
    };
    let previous = finish(&mut server, first);
    let next = rpc(
        &mut server,
        "task/start",
        json!({"sessionId":session["id"],"input":"Revised draft","artifactInputs":[previous]}),
    );
    let pending = wait(&mut server, &next["id"], "waiting_approval");
    let current = finish(&mut server, pending);
    let linked = rpc(
        &mut server,
        "artifacts/linkRevision",
        json!({"previous":previous,"current":current}),
    );
    assert_eq!(rpc(&mut server, "artifacts/getRevision", current), linked);
    assert_ne!(
        linked["previousDigest"]["sha256"],
        linked["currentDigest"]["sha256"]
    );
}
