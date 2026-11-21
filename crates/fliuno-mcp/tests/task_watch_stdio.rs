#![cfg(all(any(windows, target_os = "linux"), feature = "test-fixtures"))]

use fliuno_core::{CancellationToken, Tool, ToolContext, ToolError, ToolOutput};
use fliuno_protocol::{ToolCall, Workspace};
use serde_json::{Value, json};
use std::fs;

fn invoke(tool: &dyn Tool, session_id: &str, arguments: Value) -> Result<ToolOutput, ToolError> {
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

#[test]
fn task_watch_uses_stdio_request_ids_and_reconciles_without_cancelling_the_task() {
    for (mode, expected) in [
        ("notified", Some("notified")),
        ("ended", Some("ended")),
        ("timeout", Some("elapsed")),
        ("overbroad", Some("invalid_stream")),
        ("wrong_creation", Some("invalid_stream")),
        ("declined", Some("declined")),
        ("eof", None),
    ] {
        let root = std::env::temp_dir().join(format!(
            "fliuno-stdio-watch-{mode}-{}",
            uuid::Uuid::new_v4()
        ));
        fs::create_dir(&root).unwrap();
        let path = root.join("wire.jsonl");
        let config = serde_json::from_value(json!({
            "id":"stdio","command":env!("CARGO_BIN_EXE_fliuno-mcp-fixture"),
            "args":["--task-watch",path,mode],"cwd":root,"isolation":"trusted_host","timeoutMs":3000,
        })).unwrap();
        let tools = fliuno_mcp::connect(config).unwrap();
        let start = tools
            .iter()
            .find(|tool| tool.spec().name.starts_with("mcp_stdio_start_"))
            .unwrap();
        let watch = tools
            .iter()
            .find(|tool| tool.spec().name == "mcp_stdio_tasks_watch")
            .unwrap();
        let args = json!({"taskId":"fixture/remote-task","waitMs":500});
        assert!(matches!(
            invoke(&**watch, "owner", args.clone()),
            Err(ToolError::Failed(_))
        ));
        invoke(&**start, "owner", json!({})).unwrap();
        assert!(matches!(
            invoke(&**watch, "another", args.clone()),
            Err(ToolError::Failed(_))
        ));

        let observation = invoke(&**watch, "owner", args.clone());
        let mut expected_methods = vec![
            "server/discover",
            "tools/list",
            "tools/call",
            "tasks/get",
            "subscriptions/listen",
        ];
        if let Some(stream) = expected {
            let output = observation.unwrap().output;
            assert_eq!(output["result"]["stream"], stream, "{mode}");
            assert_eq!(
                output["result"]["snapshot"]["result"]["structuredContent"],
                42
            );
            if mode != "ended" {
                expected_methods.push("notifications/cancelled");
            }
            expected_methods.push("tasks/get");
        } else {
            assert!(matches!(observation, Err(ToolError::OutcomeUnknown(_))));
            assert!(matches!(
                invoke(&**watch, "owner", args),
                Err(ToolError::Failed(_))
            ));
        }
        drop(tools);
        let requests = fs::read_to_string(&path)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(
            requests
                .iter()
                .map(|request| request["method"].as_str().unwrap())
                .collect::<Vec<_>>(),
            expected_methods,
            "{mode}"
        );
        let subscription = &requests[4];
        let params = &subscription["params"];
        assert_eq!(
            params["notifications"],
            json!({"taskIds":["fixture/remote-task"]})
        );
        assert_eq!(
            params["_meta"]["io.modelcontextprotocol/protocolVersion"],
            "2026-07-28"
        );
        assert_eq!(
            params["_meta"]["io.modelcontextprotocol/clientCapabilities"]["extensions"]["io.modelcontextprotocol/tasks"],
            json!({})
        );
        if expected.is_some() && mode != "ended" {
            assert_eq!(requests[5]["params"]["requestId"], subscription["id"]);
            assert_ne!(requests[5]["params"]["requestId"], requests[2]["id"]);
        }
        fs::remove_file(path).unwrap();
        fs::remove_dir(root).unwrap();
    }
}
