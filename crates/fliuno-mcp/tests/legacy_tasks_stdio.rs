#![cfg(all(any(windows, target_os = "linux"), feature = "test-fixtures"))]

use fliuno_core::{CancellationToken, Tool, ToolContext, ToolError, ToolOutput};
use fliuno_protocol::{ToolCall, Workspace};
use serde_json::{Value, json};
use std::{
    fs,
    path::{Path, PathBuf},
};

fn connect(root: &Path, capability: bool) -> Vec<Box<dyn Tool>> {
    fliuno_mcp::connect(serde_json::from_value(json!({
        "id":"legacy",
        "command":PathBuf::from(env!("CARGO_BIN_EXE_fliuno-mcp-fixture")),
        "args":["--legacy-tasks",root.join("transcript.jsonl"),if capability {"capability"} else {"no-capability"}],
        "isolation":"trusted_host", "cwd":root, "timeoutMs":3000
    })).unwrap()).unwrap()
}

fn call(tool: &dyn Tool, arguments: Value, session_id: &str) -> Result<ToolOutput, ToolError> {
    tool.execute(
        &ToolContext {
            workspace: &Workspace {
                id: "workspace".into(),
                root: "unused".into(),
            },
            session_id,
            task_id: "local-task",
        },
        &ToolCall {
            id: "call".into(),
            name: tool.spec().name,
            arguments,
        },
        &CancellationToken::new(),
    )
}

fn transcript(root: &Path) -> Vec<Value> {
    fs::read_to_string(root.join("transcript.jsonl"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

fn cleanup(root: &Path) {
    fs::remove_file(root.join("transcript.jsonl")).unwrap();
    fs::remove_dir(root).unwrap();
}

#[test]
fn legacy_task_tools_keep_wire_names_and_bind_results_to_the_original_call() {
    let root = std::env::temp_dir().join(format!("fliuno-legacy-tasks-{}", uuid::Uuid::new_v4()));
    fs::create_dir(&root).unwrap();
    let tools = connect(&root, true);
    let named = |name| {
        tools
            .iter()
            .find(|tool| tool.spec().name == name)
            .unwrap()
            .as_ref()
    };
    let get = named("mcp_legacy_tasks_get");
    let result = named("mcp_legacy_tasks_result");
    let cancel = named("mcp_legacy_tasks_cancel");
    let required = tools
        .iter()
        .find(|tool| tool.spec().name.starts_with("mcp_legacy_required_"))
        .unwrap()
        .as_ref();
    let optional = tools
        .iter()
        .filter(|tool| tool.spec().name.starts_with("mcp_legacy_optional_"))
        .collect::<Vec<_>>();
    assert_eq!(optional.len(), 2);
    assert_ne!(optional[0].authority(), optional[1].authority());
    let sync = optional
        .iter()
        .find(|tool| !tool.spec().name.ends_with("_task"))
        .unwrap();
    assert_eq!(
        call(&***sync, json!({}), "owner").unwrap().output["content"][0]["text"],
        "synchronous"
    );
    let async_tool = optional
        .iter()
        .find(|tool| tool.spec().name.ends_with("_task"))
        .unwrap();
    assert_eq!(
        call(&***async_tool, json!({"case":"optional"}), "owner")
            .unwrap()
            .output["task"]["taskId"],
        "optional"
    );

    for id in ["done", "invalid", "mismatch", "working", "expired", "ttl"] {
        let handle = call(required, json!({"case":id}), "owner").unwrap().output;
        assert_eq!(handle["protocolVersion"], "2025-11-25");
        assert_eq!(handle["task"]["taskId"], id);
        assert_eq!(handle["task"]["ttl"], 15000);
        assert_eq!(handle["task"]["pollInterval"], 100);
    }
    let args = json!({"taskId":"done"});
    let before = transcript(&root).len();
    assert!(matches!(
        call(result, args.clone(), "another"),
        Err(ToolError::Failed(_))
    ));
    assert_eq!(transcript(&root).len(), before);
    let snapshot = call(get, args.clone(), "owner").unwrap().output;
    assert_eq!(snapshot["result"]["status"], "completed");
    assert!(snapshot["result"].get("structuredContent").is_none());
    assert_eq!(
        call(result, args, "owner").unwrap().output["result"]["structuredContent"]["answer"],
        42
    );
    for id in ["invalid", "mismatch"] {
        assert!(matches!(call(result, json!({"taskId":id}), "owner"),
            Err(ToolError::OutcomeUnknownWithOutput { output, message }) if output == json!({"taskId":id,"status":"completed"}) && !message.contains("invalid private data")));
    }
    assert!(
        matches!(call(result, json!({"taskId":"working"}), "owner"), Err(ToolError::Failed(message)) if message.contains("terminal"))
    );
    assert!(
        matches!(call(get, json!({"taskId":"expired"}), "owner"), Err(ToolError::Failed(message)) if message.contains("read request"))
    );
    assert!(
        matches!(call(cancel, json!({"taskId":"expired"}), "owner"), Err(ToolError::Failed(message)) if message.contains("no cancellation request sent"))
    );
    for tool in [result, cancel] {
        assert!(
            matches!(call(tool, json!({"taskId":"ttl"}), "owner"), Err(ToolError::Failed(message)) if message.contains("TTL elapsed"))
        );
    }
    let waited = call(
        named("mcp_legacy_tasks_wait"),
        json!({"taskId":"ttl","waitMs":100}),
        "owner",
    )
    .unwrap();
    assert_eq!(waited.output["result"]["reason"], "ttl_elapsed");
    assert_eq!(
        call(cancel, json!({"taskId":"working"}), "owner")
            .unwrap()
            .output["result"]["status"],
        "cancelled"
    );

    assert!(matches!(
        call(required, json!({"case":"lost"}), "owner"),
        Err(ToolError::OutcomeUnknown(_))
    ));
    assert!(matches!(
        call(required, json!({"case":"lost"}), "owner"),
        Err(ToolError::Failed(_))
    ));
    let log = transcript(&root);
    assert!(
        !log.iter()
            .any(|message| message["method"] == "tasks/update")
    );
    assert_eq!(
        log.iter()
            .filter(|message| message["method"] == "tools/call"
                && message["params"]["arguments"]["case"] == "lost")
            .count(),
        1
    );
    assert_eq!(
        log.iter()
            .filter(|message| message["method"] == "tasks/result")
            .count(),
        3
    );
    assert_eq!(
        log.iter()
            .filter(|message| message["method"] == "tasks/cancel")
            .count(),
        1
    );
    drop(tools);
    cleanup(&root);
}

#[test]
fn legacy_tool_task_support_without_the_server_capability_never_dispatches_tasks() {
    let root = std::env::temp_dir().join(format!(
        "fliuno-legacy-task-capability-{}",
        uuid::Uuid::new_v4()
    ));
    fs::create_dir(&root).unwrap();
    let tools = connect(&root, false);
    assert_eq!(tools.len(), 2);
    for tool in &tools {
        assert!(!tool.spec().name.contains("required"));
        assert_eq!(
            call(tool.as_ref(), json!({}), "owner").unwrap().output["content"][0]["text"],
            "synchronous"
        );
    }
    assert!(!transcript(&root).iter().any(
        |message| message["method"] == "tools/call" && message["params"].get("task").is_some()
    ));
    drop(tools);
    cleanup(&root);
}

#[test]
fn legacy_wait_checks_notification_identity_and_reconciles_with_get() {
    let root = std::env::temp_dir().join(format!("fliuno-legacy-watch-{}", uuid::Uuid::new_v4()));
    fs::create_dir(&root).unwrap();
    let tools = connect(&root, true);
    let named = |prefix: &str| {
        tools
            .iter()
            .find(|tool| tool.spec().name.starts_with(prefix))
            .unwrap()
            .as_ref()
    };
    let start = named("mcp_legacy_required_");
    let wait = named("mcp_legacy_tasks_wait");
    assert_ne!(wait.authority(), named("mcp_legacy_tasks_get").authority());
    for (id, wait_ms, reason, status, polls, notification) in [
        ("notify", 1200, "input_required", "input_required", 2, true),
        ("unrelated", 100, "wait_budget", "working", 1, false),
    ] {
        call(start, json!({"case":id}), "owner").unwrap();
        let args = json!({"taskId":id,"waitMs":wait_ms});
        let before = transcript(&root).len();
        assert!(matches!(
            call(wait, args.clone(), "another"),
            Err(ToolError::Failed(_))
        ));
        assert_eq!(transcript(&root).len(), before);
        let result = call(wait, args, "owner").unwrap().output;
        assert_eq!(result["result"]["reason"], reason);
        assert_eq!(result["result"]["snapshot"]["status"], status);
        assert_eq!(result["result"]["polls"], polls);
        assert_eq!(result["result"]["notificationReceived"], notification);
        let log = transcript(&root);
        assert_eq!(log.len() - before, polls);
        assert!(
            log[before..]
                .iter()
                .all(|message| message["method"] == "tasks/get")
        );
    }
    drop(tools);
    cleanup(&root);
}
