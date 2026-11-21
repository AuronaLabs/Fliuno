use super::*;
use fliuno_core::{DemoProvider, Kernel};
use std::fs;

#[test]
fn reopened_database_restores_contract_ownership_and_spent_inputs() {
    let phase = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let remote_phase = phase.clone();
    let fixture = Fixture::start(move |request| {
        let phase = remote_phase.load(Ordering::Relaxed);
        let mut task = json!({"taskId":"retained","status":"working",
            "createdAt":"2026-11-21T00:00:00Z","lastUpdatedAt":"2026-11-21T00:00:00Z","ttlMs":null});
        let result = match request.body["method"].as_str() {
            Some("server/discover") => json!({"resultType":"complete",
                "supportedVersions":["2026-07-28"],"ttlMs":0,"cacheScope":"private",
                "capabilities":{"tools":{},"extensions":{"io.modelcontextprotocol/tasks":{}}}}),
            Some("tools/list") => json!({"tools":[{"name":"echo","inputSchema":{"type":"object"},
                "outputSchema":{"type":if phase == 0 {"integer"} else {"string"}}}]}),
            Some("tools/call") => {
                task["resultType"] = json!("task");
                task
            }
            Some("tasks/get") => {
                task["resultType"] = json!("complete");
                if phase < 2 || phase == 4 {
                    task["status"] = json!("input_required");
                    task["inputRequests"] = json!({"form":{"method":"elicitation/create",
                        "params":{"mode":"form","message":"Continue?",
                            "requestedSchema":{"type":"object","properties":{}}}}});
                    if phase == 4 {
                        task["ttlMs"] = json!(1);
                    }
                } else {
                    task["status"] = json!("completed");
                    task["result"] = json!({"content":[],"structuredContent":
                        if phase == 2 {json!(42)} else {json!("new schema only")}});
                }
                task
            }
            Some("tasks/update") => json!({"resultType":"complete"}),
            _ => return wire::reply(request, Version::Modern),
        };
        Response::json(
            200,
            json!({"jsonrpc":"2.0","id":request.body["id"],"result":result}),
        )
    });

    let root = std::env::temp_dir().join(format!("fliuno-task-binding-{}", uuid::Uuid::new_v4()));
    fs::create_dir(&root).unwrap();
    let db = root.join("kernel.sqlite");
    let open = || Kernel::open(&db, Box::new(DemoProvider)).unwrap();
    let register = |kernel: &Kernel| {
        crate::connect_with_store(
            ServerConfig::Http(config(&fixture)),
            kernel.mcp_task_store().unwrap(),
            None,
        )
        .unwrap()
    };
    let mut kernel = open();
    let workspace = kernel.open_workspace(root.to_str().unwrap()).unwrap();
    let owner = kernel.create_session(&workspace.id).unwrap();
    let other = kernel.create_session(&workspace.id).unwrap();
    let task = kernel.start_task(&owner.id, "fixture").unwrap();
    let call = |tools: &[Box<dyn Tool>], suffix: &str, session_id: &str, args: Value| {
        let tool = tools
            .iter()
            .find(|tool| {
                let name = tool.spec().name;
                name.ends_with(suffix) || (suffix == "echo" && name.starts_with("mcp_remote_echo_"))
            })
            .unwrap();
        tool.execute(
            &ToolContext {
                workspace: &workspace,
                task_id: &task.id,
                session_id,
            },
            &ToolCall {
                id: "call".into(),
                name: tool.spec().name,
                arguments: args,
            },
            &CancellationToken::new(),
        )
    };
    let tools = register(&kernel);
    let answer =
        json!({"taskId":"retained","inputResponses":{"form":{"action":"accept","content":{}}}});
    assert_eq!(
        call(&tools, "echo", &owner.id, json!({})).unwrap().output["taskId"],
        "retained"
    );
    call(&tools, "tasks_update", &owner.id, answer.clone()).unwrap();
    drop(tools);
    drop(kernel);

    phase.store(1, Ordering::Relaxed);
    let kernel = open();
    let tools = register(&kernel);
    let before = fixture.requests().len();
    let request = json!({"taskId":"retained"});
    assert!(
        matches!(call(&tools, "tasks_get", &other.id, request.clone()), Err(ToolError::Failed(message)) if message.contains("different workspace or session"))
    );
    assert_eq!(fixture.requests().len(), before);
    assert!(
        matches!(call(&tools, "tasks_update", &owner.id, answer.clone()), Err(ToolError::Failed(message)) if message.contains("already sent"))
    );
    phase.store(2, Ordering::Relaxed);
    assert_eq!(
        call(&tools, "tasks_get", &owner.id, request.clone())
            .unwrap()
            .output["result"]["result"]["structuredContent"],
        42
    );
    phase.store(3, Ordering::Relaxed);
    assert!(
        matches!(call(&tools, "tasks_get", &owner.id, request.clone()), Err(ToolError::OutcomeUnknownWithOutput { message, .. }) if message.contains("original tool schema"))
    );
    phase.store(4, Ordering::Relaxed);
    assert!(
        matches!(call(&tools, "tasks_update", &owner.id, answer), Err(ToolError::Failed(message)) if message.contains("TTL elapsed"))
    );
    assert!(
        matches!(call(&tools, "tasks_cancel", &owner.id, request), Err(ToolError::Failed(message)) if message.contains("TTL elapsed"))
    );
    let wait = json!({"taskId":"retained","waitMs":100});
    for (tool, field) in [("tasks_wait", "reason"), ("tasks_watch", "stream")] {
        assert_eq!(
            call(&tools, tool, &owner.id, wait.clone()).unwrap().output["result"][field],
            "ttl_elapsed"
        );
    }
    assert!(fixture.requests().iter().all(|r| !matches!(
        r.body["method"].as_str(),
        Some("tasks/cancel" | "subscriptions/listen")
    )));
    for method in ["tools/call", "tasks/update"] {
        assert_eq!(
            fixture
                .requests()
                .iter()
                .filter(|r| r.body["method"] == method)
                .count(),
            1
        );
    }
    drop(tools);
    drop(kernel);
    for entry in fs::read_dir(&root).unwrap() {
        fs::remove_file(entry.unwrap().path()).unwrap();
    }
    fs::remove_dir(root).unwrap();
}
