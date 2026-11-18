#[cfg(all(any(windows, target_os = "linux"), feature = "test-fixtures"))]
#[test]
fn actual_mcp_stdio_remains_isolated_and_timeout_kills_the_sandbox() {
    use fliuno_core::{CancellationToken, ToolContext, ToolError};
    use fliuno_protocol::{ToolCall, Workspace};
    use serde_json::{Value, json};
    use std::{fs, net::TcpListener, path::PathBuf, process::Command};
    let executable = PathBuf::from(env!("CARGO_BIN_EXE_fliuno-mcp-fixture"));
    let root = std::env::temp_dir().join(format!("fliuno-mcp-native-{}", std::process::id()));
    fs::create_dir_all(&root).unwrap();
    let outside = root.join("secret.txt");
    fs::write(&outside, "private").unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = listener.local_addr().unwrap().to_string();
    let control = Command::new(&executable)
        .arg("--probe-native")
        .arg(&outside)
        .arg(&endpoint)
        .current_dir(&root)
        .output()
        .unwrap();
    assert!(
        control.status.success(),
        "{}",
        String::from_utf8_lossy(&control.stderr)
    );
    let control: Value = serde_json::from_slice(&control.stdout).unwrap();
    for field in ["read", "write", "network", "insideWrite"] {
        assert_eq!(control[field], true);
    }
    fs::write(&outside, "private").unwrap();
    let isolation = if cfg!(windows) {
        "windows_app_container"
    } else {
        "linux_landlock"
    };
    // Initialization includes private profile setup/staging. Five seconds was
    // below observed native cold-start time on the Windows verification host.
    // Use the production default; the separate delayed call still exceeds it
    // and must time out without reopening or replaying the host.
    let initialization = std::time::Instant::now();
    let tools = fliuno_mcp::connect(
        serde_json::from_value(
            json!({"id":"probe","command":executable,"isolation":isolation,"timeoutMs":30000}),
        )
        .unwrap(),
    )
    .unwrap_or_else(|error| {
        panic!(
            "isolated MCP initialization failed after {:?}: {error}",
            initialization.elapsed()
        )
    });
    eprintln!(
        "isolated MCP initialized in {} ms",
        initialization.elapsed().as_millis()
    );
    let tool = &tools[0];
    let workspace = Workspace {
        id: "ws".into(),
        root: root.to_string_lossy().into_owned(),
    };
    let context = ToolContext {
        session_id: "session_test",
        task_id: "test",
        workspace: &workspace,
    };
    let mut call = ToolCall {
        id: "call".into(),
        name: tool.spec().name,
        arguments: json!({"hostPath":outside,"endpoint":endpoint}),
    };
    let result = tool
        .execute(&context, &call, &CancellationToken::new())
        .unwrap();
    let report: Value =
        serde_json::from_str(result.output["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(report["container"], cfg!(windows));
    assert_eq!(report["insideWrite"], true);
    for field in ["read", "write", "network"] {
        assert_eq!(report[field], false);
    }
    assert_eq!(fs::read_to_string(&outside).unwrap(), "private");
    let private_directory = PathBuf::from(report["cwd"].as_str().unwrap());
    assert!(private_directory.exists());
    call.arguments["delayMs"] = json!(60000);
    tool.validate(&call.arguments).unwrap();
    assert!(matches!(
        tool.execute(&context, &call, &CancellationToken::new()),
        Err(ToolError::OutcomeUnknown(_))
    ));
    assert!(
        !private_directory.exists(),
        "timeout left sandbox storage/process alive"
    );
    assert!(matches!(
        tool.execute(&context, &call, &CancellationToken::new()),
        Err(ToolError::Failed(_))
    ));
    drop(tools);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn isolation_is_explicit_and_conflicting_cwd_is_rejected() {
    use serde_json::json;
    let executable = std::env::current_exe().unwrap();
    assert!(
        serde_json::from_value::<fliuno_mcp::StdioServerConfig>(
            json!({"id":"test","command":executable,"cwd":std::env::temp_dir()})
        )
        .is_err()
    );
    let config = serde_json::from_value(json!({"id":"test","command":executable,"isolation":"windows_app_container","cwd":std::env::temp_dir()})).unwrap();
    assert!(
        fliuno_mcp::connect(config)
            .err()
            .unwrap()
            .contains("omit cwd")
    );
}
