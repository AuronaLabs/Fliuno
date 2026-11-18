//! A real SDK peer, native isolation, and SQLite approvals across restarts.
#![cfg(all(any(windows, target_os = "linux"), feature = "test-fixtures"))]
use fliuno_core::{CancellationToken, Kernel, ModelDecision, ModelProvider, ModelTurn, ToolSpec};
use fliuno_protocol::{ApprovalDecision, Message, Task, TaskStatus, ToolCall};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    fs,
    net::TcpListener,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};

struct Directory(PathBuf);
impl Directory {
    fn new() -> Self {
        static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "fliuno-mcp-bundle-{}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&root).unwrap();
        fs::write(root.join("private.txt"), "host bytes").unwrap();
        Self(root)
    }
    fn config(&self, bytes: &[u8]) -> fliuno_mcp::StdioServerConfig {
        let source = self.0.join("source.txt");
        fs::write(&source, bytes).unwrap();
        serde_json::from_value(json!({
            "id":"bundle", "command":env!("CARGO_BIN_EXE_fliuno-mcp-fixture"),
            "isolation":if cfg!(windows) { "windows_app_container" } else { "linux_landlock" },
            "assets":[{"source":source, "path":"data/resource.txt", "sha256":format!("{:x}",Sha256::digest(bytes))}]
        })).unwrap()
    }
}
impl Drop for Directory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
struct Provider(Value);
impl ModelProvider for Provider {
    fn next(
        &self,
        task: &Task,
        tools: &[ToolSpec],
        _: &CancellationToken,
    ) -> Result<ModelTurn, String> {
        if task
            .messages
            .iter()
            .any(|message| matches!(message, Message::ToolResult { .. }))
        {
            return Ok(ModelDecision::Complete("observed".into()).into());
        }
        Ok(ModelDecision::Call(ToolCall {
            id: format!("call_{}", task.id),
            name: tools
                .iter()
                .find(|tool| tool.name.starts_with("mcp_bundle_"))
                .unwrap()
                .name
                .clone(),
            arguments: self.0.clone(),
        })
        .into())
    }
}
fn register(kernel: &mut Kernel, config: fliuno_mcp::StdioServerConfig) {
    fliuno_mcp::register_config(
        kernel,
        fliuno_mcp::McpConfig {
            servers: vec![fliuno_mcp::ServerConfig::Stdio(config)],
        },
    )
    .unwrap();
}
fn approve(kernel: &mut Kernel, task: Task) -> Task {
    assert_eq!(task.status, TaskStatus::WaitingApproval, "{task:?}");
    kernel
        .respond_approval(
            &task.id,
            &task.pending_approval.unwrap().id,
            ApprovalDecision::AllowOnce,
        )
        .unwrap()
}
fn report(task: &Task) -> Value {
    assert_eq!(task.status, TaskStatus::Completed, "{task:?}");
    let output = task
        .messages
        .iter()
        .find_map(|message| match message {
            Message::ToolResult { output, .. } => Some(output),
            _ => None,
        })
        .unwrap();
    serde_json::from_str(output["content"][0]["text"].as_str().unwrap()).unwrap()
}

#[test]
fn changed_bundle_cannot_reuse_persisted_approval_and_fresh_consent_reads_new_copy() {
    let directory = Directory::new();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let arguments = json!({"hostPath":directory.0.join("private.txt"),
        "endpoint":listener.local_addr().unwrap().to_string(), "stagedPath":"data/resource.txt"});
    let database = directory.0.join("kernel.db");
    let mut kernel = Kernel::open(&database, Box::new(Provider(arguments.clone()))).unwrap();
    register(&mut kernel, directory.config("第一版".as_bytes()));
    let workspace = kernel
        .open_workspace(directory.0.to_str().unwrap())
        .unwrap();
    let session = kernel.create_session(&workspace.id).unwrap();
    let first = kernel
        .start_task(&session.id, "read approved companion")
        .unwrap();
    let first = approve(&mut kernel, first);
    let first = report(&first);
    assert_eq!(first["asset"], "第一版");
    assert_eq!(first["calls"], 1);
    assert_eq!(first["insideWrite"], true);
    for field in ["read", "write", "network"] {
        assert_eq!(first[field], false);
    }
    let waiting = kernel
        .start_task(&session.id, "pending previous bundle")
        .unwrap();
    let old_name = waiting.pending_approval.as_ref().unwrap().call.name.clone();
    drop(kernel);

    let mut kernel = Kernel::open(&database, Box::new(Provider(arguments))).unwrap();
    register(&mut kernel, directory.config("第二版".as_bytes()));
    assert!(!kernel.tool_specs().iter().any(|tool| tool.name == old_name));
    let failed = approve(&mut kernel, waiting);
    assert_eq!(failed.status, TaskStatus::Failed, "{failed:?}");
    let fresh = kernel
        .start_task(&session.id, "new bundle with fresh consent")
        .unwrap();
    let fresh = approve(&mut kernel, fresh);
    let fresh = report(&fresh);
    assert_eq!(fresh["asset"], "第二版");
    // The rejected old approval never reached the freshly started SDK server.
    assert_eq!(fresh["calls"], 1);
    assert_eq!(
        fs::read_to_string(directory.0.join("private.txt")).unwrap(),
        "host bytes"
    );
}

#[test]
fn changed_source_is_rejected_before_sdk_initialization_without_host_fallback() {
    let directory = Directory::new();
    let config = directory.config(b"pinned");
    fs::write(&config.assets[0].source, b"tampered").unwrap();
    let error = fliuno_mcp::connect(config).err().unwrap();
    // The async launcher exposes a phase and withholds the underlying path and
    // server bytes. Digest-specific diagnostics are tested in sandbox itself.
    assert!(error.contains("MCP sandbox launch failed"), "{error}");
}

#[test]
fn trusted_host_does_not_accept_private_resource_manifests() {
    let directory = Directory::new();
    let mut config = directory.config(b"resource");
    config.isolation = fliuno_mcp::ProcessIsolation::TrustedHost;
    config.cwd = Some(directory.0.clone());
    assert!(fliuno_mcp::connect(config).is_err());
}

#[test]
fn unbundled_stdio_keeps_alias_but_changed_arguments_invalidate_pending_consent() {
    let directory = Directory::new();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let arguments = json!({"hostPath":directory.0.join("private.txt"),"endpoint":listener.local_addr().unwrap().to_string()});
    let database = directory.0.join("kernel.db");
    let mut config = directory.config(b"not staged");
    config.assets.clear();
    let mut kernel = Kernel::open(&database, Box::new(Provider(arguments.clone()))).unwrap();
    register(&mut kernel, config);
    let workspace = kernel
        .open_workspace(directory.0.to_str().unwrap())
        .unwrap();
    let session = kernel.create_session(&workspace.id).unwrap();
    let waiting = kernel.start_task(&session.id, "original argv").unwrap();
    let alias = waiting.pending_approval.as_ref().unwrap().call.name.clone();
    drop(kernel);
    let mut config = directory.config(b"not staged");
    config.assets.clear();
    config.args = vec!["--different-host-argument".into()];
    let mut kernel = Kernel::open(&database, Box::new(Provider(arguments))).unwrap();
    register(&mut kernel, config);
    assert!(kernel.tool_specs().iter().any(|tool| tool.name == alias));
    assert_eq!(approve(&mut kernel, waiting).status, TaskStatus::Failed);
    let fresh = kernel
        .start_task(&session.id, "fresh argv consent")
        .unwrap();
    let fresh = approve(&mut kernel, fresh);
    assert_eq!(report(&fresh)["calls"], 1);
}
