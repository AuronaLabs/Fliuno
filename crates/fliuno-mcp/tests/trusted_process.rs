//! Run real MCP stdio through the trusted native process owner.
#![cfg(all(any(windows, target_os = "linux"), feature = "test-fixtures"))]
use fliuno_core::{CancellationToken, ToolContext};
use fliuno_mcp::StdioServerConfig;
use fliuno_protocol::{ToolCall, Workspace};
use serde_json::{Value, json};
use std::{fs, net::TcpListener, path::PathBuf};

struct Directory(PathBuf);
impl Directory {
    fn new() -> Self {
        let directory = std::env::temp_dir().join(format!(
            "fliuno-mcp-trusted-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        fs::create_dir(&directory).unwrap();
        Self(directory)
    }

    fn config(&self) -> StdioServerConfig {
        serde_json::from_value(json!({
            "id":"native",
            "command":env!("CARGO_BIN_EXE_fliuno-mcp-fixture"),
            "cwd":self.0,
            "isolation":"trusted_host",
        }))
        .unwrap()
    }

    fn check_tool(&self, tools: &[Box<dyn fliuno_core::Tool>]) -> Value {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let host_file = self.0.join("host.txt");
        fs::write(&host_file, "host").unwrap();
        let tool = &tools[0];
        let output = tool
            .execute(
                &ToolContext {
                    session_id: "s",
                    task_id: "t",
                    workspace: &Workspace {
                        id: "w".into(),
                        root: self.0.to_string_lossy().into_owned(),
                    },
                },
                &ToolCall {
                    id: "check".into(),
                    name: tool.spec().name,
                    arguments: json!({
                        "hostPath":host_file,
                        "endpoint":listener.local_addr().unwrap().to_string(),
                        "probeChild":true
                    }),
                },
                &CancellationToken::new(),
            )
            .unwrap();
        let report: Value =
            serde_json::from_str(output.output["content"][0]["text"].as_str().unwrap()).unwrap();
        for field in ["read", "write", "network", "insideWrite"] {
            assert_eq!(report[field], true);
        }
        assert_eq!(report["container"], false);
        assert_eq!(fs::read_to_string(host_file).unwrap(), "fixture modified");
        report
    }
}
impl Drop for Directory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn trusted_stdio_keeps_explicit_host_access() {
    let directory = Directory::new();
    let tools = fliuno_mcp::connect(directory.config()).unwrap();
    assert_eq!(directory.check_tool(&tools)["child"], true);
}

#[cfg(windows)]
#[test]
fn trusted_stdio_enforces_an_explicit_windows_job_process_limit() {
    let directory = Directory::new();
    let mut config = directory.config();
    config.resources.pids_max = Some(1);
    let tools = fliuno_mcp::connect(config).unwrap();
    assert_eq!(directory.check_tool(&tools)["child"], false);
}

#[cfg(target_os = "linux")]
#[test]
fn trusted_stdio_rejects_limits_without_a_delegated_cgroup() {
    let directory = Directory::new();
    let mut config = directory.config();
    config.linux_cgroup_parent = Some(directory.0.clone());
    config.resources.pids_max = Some(16);
    let error = fliuno_mcp::connect(config).err().unwrap();
    assert!(error.contains("cannot spawn trusted MCP server"), "{error}");
}

#[cfg(target_os = "linux")]
#[test]
fn trusted_stdio_applies_limits_inside_an_explicit_linux_cgroup() {
    let Some(parent) = std::env::var_os("FLIUNO_MCP_TEST_CGROUP_PARENT").map(PathBuf::from) else {
        eprintln!("set FLIUNO_MCP_TEST_CGROUP_PARENT for delegated Linux validation");
        return;
    };
    let directory = Directory::new();
    let mut config = directory.config();
    config.linux_cgroup_parent = Some(parent.clone());
    config.resources.pids_max = Some(16);
    config.resources.memory_max_bytes = Some(256 * 1024 * 1024);
    let tools = fliuno_mcp::connect(config).unwrap();
    let cgroups: Vec<_> = fs::read_dir(&parent)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|entry| {
            entry
                .file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("fliuno-")
        })
        .collect();
    assert_eq!(cgroups.len(), 1);
    let group = &cgroups[0];
    assert_eq!(
        fs::read_to_string(group.join("pids.max")).unwrap().trim(),
        "16"
    );
    assert_eq!(
        fs::read_to_string(group.join("memory.max")).unwrap().trim(),
        "268435456"
    );
    assert!(
        !fs::read_to_string(group.join("cgroup.procs"))
            .unwrap()
            .trim()
            .is_empty()
    );
    assert_eq!(directory.check_tool(&tools)["child"], true);
    drop(tools);
    assert!(!group.exists(), "MCP drop left a delegated group alive");
}
