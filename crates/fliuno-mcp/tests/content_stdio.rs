//! One real SDK binary uses the same content channel in host and private modes.
#![cfg(all(any(windows, target_os = "linux"), feature = "test-fixtures"))]
use fliuno_core::{CancellationToken, ToolContext};
use fliuno_protocol::{ToolCall, Workspace};
use serde_json::{Value, json};
use std::path::PathBuf;

#[test]
fn content_only_stdio_process_interoperates_in_host_and_isolated_modes() {
    let executable = PathBuf::from(env!("CARGO_BIN_EXE_fliuno-mcp-fixture"));
    let root = std::env::current_dir().unwrap();
    for isolated in [false, true] {
        let isolation = if !isolated {
            "trusted_host"
        } else if cfg!(windows) {
            "windows_app_container"
        } else {
            "linux_landlock"
        };
        let tools = fliuno_mcp::connect(
            serde_json::from_value(json!({
                "id":"content","command":executable,"args":["--content"],"isolation":isolation,
                "cwd":if isolated {None} else {Some(&root)},"timeoutMs":30000
            }))
            .unwrap(),
        )
        .unwrap();
        assert_eq!(tools.len(), 5);
        let mut private_directory = None;
        for (label, arguments, field) in [
            ("resources_list", json!({}), "resources"),
            ("resources_templates", json!({}), "resourceTemplates"),
            (
                "resources_read",
                json!({"uri":"fixture:environment"}),
                "contents",
            ),
            ("prompts_list", json!({}), "prompts"),
            ("prompts_get", json!({"name":"draft"}), "messages"),
        ] {
            let tool = tools
                .iter()
                .find(|tool| tool.spec().name == format!("mcp_content_{label}"))
                .unwrap();
            let result = tool
                .execute(
                    &ToolContext {
                        session_id: "s",
                        task_id: "t",
                        workspace: &Workspace {
                            id: "w".into(),
                            root: root.to_string_lossy().into_owned(),
                        },
                    },
                    &ToolCall {
                        id: "c".into(),
                        name: tool.spec().name,
                        arguments,
                    },
                    &CancellationToken::new(),
                )
                .unwrap();
            assert_eq!(result.output["result"][field].as_array().unwrap().len(), 1);
            if label == "resources_read" {
                let environment: Value = serde_json::from_str(
                    result.output["result"]["contents"][0]["text"]
                        .as_str()
                        .unwrap(),
                )
                .unwrap();
                assert_ne!(
                    environment["pid"].as_u64().unwrap(),
                    u64::from(std::process::id())
                );
                assert_eq!(environment["container"], cfg!(windows) && isolated);
                let cwd = PathBuf::from(environment["cwd"].as_str().unwrap());
                if isolated {
                    assert_ne!(
                        std::fs::canonicalize(&cwd).unwrap(),
                        std::fs::canonicalize(&root).unwrap()
                    );
                    private_directory = Some(cwd);
                } else {
                    assert_eq!(
                        std::fs::canonicalize(cwd).unwrap(),
                        std::fs::canonicalize(&root).unwrap()
                    );
                }
            }
        }
        drop(tools);
        if let Some(directory) = private_directory {
            assert!(
                !directory.exists(),
                "content connection leaked private storage"
            );
        }
    }
}
