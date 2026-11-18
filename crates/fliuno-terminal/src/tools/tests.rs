//! Authority and crash boundaries without starting an interactive shell.
use super::*;
use crate::config::Isolation;
use fliuno_core::{DemoProvider, ModelDecision, ModelProvider, ModelTurn};
use fliuno_protocol::{ApprovalDecision, Task, TaskStatus};
use std::{fs, io::Write, path::Path};

struct Directory(PathBuf);
impl Directory {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "fliuno-terminal-authority-{}",
            uuid::Uuid::new_v4()
        ));
        fs::create_dir(&root).unwrap();
        Self(root)
    }
}
impl Drop for Directory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn preset() -> crate::PresetConfig {
    crate::PresetConfig {
        id: "probe".into(),
        command: std::env::current_exe().unwrap(),
        isolation: Isolation::TrustedHost,
        args: vec![],
        env_allow: vec![],
        lifetime_ms: 30000,
        spool_bytes: 65536,
        resources: Default::default(),
        linux_cgroup_parent: None,
    }
}
fn config(root: &Path, preset: crate::PresetConfig) -> Config {
    Config {
        state_directory: root.join("terminals"),
        presets: vec![preset],
    }
}
struct StartProvider;
impl ModelProvider for StartProvider {
    fn next(
        &self,
        _: &Task,
        tools: &[ToolSpec],
        _: &CancellationToken,
    ) -> Result<ModelTurn, String> {
        let tool = tools
            .iter()
            .find(|t| t.name.starts_with("terminal_probe_"))
            .unwrap();
        Ok(ModelDecision::Call(ToolCall {
            id: "create".into(),
            name: tool.name.clone(),
            arguments: json!({}),
        })
        .into())
    }
}

#[test]
fn configuration_validation_is_atomic_and_requires_explicit_authority() {
    let directory = Directory::new();
    let mut kernel = Kernel::in_memory(Box::new(DemoProvider)).unwrap();
    let before = kernel.tool_specs().len();
    let mut invalid = preset();
    invalid.id = "bad id".into();
    let mut configuration = config(&directory.0, preset());
    configuration.presets.push(invalid);
    assert!(register_config(&mut kernel, configuration, std::env::current_exe().unwrap()).is_err());
    assert_eq!(kernel.tool_specs().len(), before);
    for input in [
        json!({"stateDirectory":directory.0,"presets":[{"id":"missing","command":std::env::current_exe().unwrap()}]}),
        json!({"stateDirectory":directory.0,"presets":[],"fallbackToHost":true}),
        json!({"stateDirectory":directory.0,"presets":[{"id":"bad","command":"relative.exe","isolation":"trusted_host","cwd":"replacement"}]}),
    ] {
        assert!(serde_json::from_value::<Config>(input).is_err());
    }
    register_config(
        &mut kernel,
        config(&directory.0, preset()),
        std::env::current_exe().unwrap(),
    )
    .unwrap();
    let after = kernel.tool_specs().len();
    assert_eq!(after, before + 9);
    // A collision must not register any of the new presets preceding it.
    let mut extra = preset();
    extra.id = "extra".into();
    let mut repeated = config(&directory.0, extra);
    repeated.presets.push(preset());
    assert!(register_config(&mut kernel, repeated, std::env::current_exe().unwrap()).is_err());
    assert_eq!(kernel.tool_specs().len(), after);
}

#[test]
fn terminal_resource_budgets_are_validated_and_bound_to_preset_identity() {
    let directory = Directory::new();
    let service = Service::new(
        directory.0.join("terminals"),
        std::env::current_exe().unwrap(),
    )
    .unwrap();
    let unrestricted = Preset::resolve(preset(), &service.root).unwrap();

    let mut limited_config = preset();
    limited_config.resources.memory_max_bytes = Some(256 * 1024 * 1024);
    limited_config.resources.pids_max = Some(64);
    let limited = Preset::resolve(limited_config, &service.root).unwrap();
    assert_ne!(unrestricted.identity, limited.identity);

    let mut invalid = preset();
    invalid.resources.pids_max = Some(0);
    assert!(Preset::resolve(invalid, &service.root).is_err());

    let mut invalid_parent = preset();
    invalid_parent.linux_cgroup_parent = Some(PathBuf::from("relative-cgroup"));
    assert!(Preset::resolve(invalid_parent, &service.root).is_err());
}

#[test]
fn model_input_cannot_replace_host_authority_or_exceed_byte_budgets() {
    let directory = Directory::new();
    let service = Service::new(
        directory.0.join("terminals"),
        std::env::current_exe().unwrap(),
    )
    .unwrap();
    let start = Start {
        preset: Arc::new(Preset::resolve(preset(), &service.root).unwrap()),
        name: "unused".into(),
        service: service.clone(),
    };
    for field in [
        "command",
        "cwd",
        "env",
        "isolation",
        "sessionId",
        "lifetimeMs",
        "spoolBytes",
    ] {
        let mut arguments = json!({});
        arguments[field] = json!("replacement");
        assert!(start.validate(&arguments).is_err(), "{field}");
    }
    for arguments in [
        json!({"rows":0}),
        json!({"cols":501}),
        json!({"args":["\u{0000}"]}),
        json!({"args":["x".repeat(32001)]}),
    ] {
        assert!(start.validate(&arguments).is_err());
    }
    assert!(
        start
            .validate(&json!({"rows":1,"cols":500,"args":["literal; value"]}))
            .is_ok()
    );
    let write = Control {
        kind: Kind::Write,
        service: service.clone(),
        name: "unused".into(),
    };
    let terminal = "a".repeat(64);
    for data in [
        String::new(),
        "x".repeat(INPUT_LIMIT + 1),
        "中".repeat(INPUT_LIMIT / 3 + 1),
    ] {
        assert!(
            write
                .validate(&json!({"terminalId":terminal,"data":data}))
                .is_err()
        );
    }
    assert!(
        write
            .validate(&json!({"terminalId":terminal,"data":"中".repeat(INPUT_LIMIT/3)}))
            .is_ok()
    );
    assert!(
        write
            .validate(&json!({"terminalId":"../outside","data":"x"}))
            .is_err()
    );
    let output = Control {
        kind: Kind::Output,
        service,
        name: "unused".into(),
    };
    for arguments in [
        json!({"terminalId":terminal,"limit":0}),
        json!({"terminalId":terminal,"limit":READ_LIMIT+1}),
        json!({"terminalId":terminal,"after":u64::MAX}),
        json!({"terminalId":terminal,"sessionId":"other"}),
    ] {
        assert!(output.validate(&arguments).is_err());
    }
}

#[test]
fn executable_change_before_approval_is_a_known_failure_without_process_creation() {
    let directory = Directory::new();
    let command = directory.0.join(if cfg!(windows) {
        "program.exe"
    } else {
        "program"
    });
    fs::copy(std::env::current_exe().unwrap(), &command).unwrap();
    let mut selected = preset();
    selected.command = command.clone();
    let mut kernel = Kernel::in_memory(Box::new(StartProvider)).unwrap();
    let service = register_config(
        &mut kernel,
        config(&directory.0, selected),
        std::env::current_exe().unwrap(),
    )
    .unwrap();
    let workspace = kernel
        .open_workspace(directory.0.to_str().unwrap())
        .unwrap();
    let session = kernel.create_session(&workspace.id).unwrap();
    let pending = kernel.start_task(&session.id, "start").unwrap();
    assert_eq!(pending.status, TaskStatus::WaitingApproval);
    fs::OpenOptions::new()
        .append(true)
        .open(command)
        .unwrap()
        .write_all(b"changed")
        .unwrap();
    let result = kernel
        .respond_approval(
            &pending.id,
            &pending.pending_approval.unwrap().id,
            ApprovalDecision::AllowOnce,
        )
        .unwrap();
    assert_eq!(result.status, TaskStatus::Failed);
    assert!(result.output.unwrap().contains("executable changed"));
    assert!(
        runtime()
            .unwrap()
            .block_on(service.list(&session.id))
            .unwrap()
            .is_empty()
    );
}

#[test]
fn changed_configuration_cannot_rebind_a_durable_pending_approval() {
    let directory = Directory::new();
    let database = directory.0.join("kernel.db");
    let mut kernel = Kernel::open(&database, Box::new(StartProvider)).unwrap();
    register_config(
        &mut kernel,
        config(&directory.0, preset()),
        std::env::current_exe().unwrap(),
    )
    .unwrap();
    let workspace = kernel
        .open_workspace(directory.0.to_str().unwrap())
        .unwrap();
    let session = kernel.create_session(&workspace.id).unwrap();
    let pending = kernel.start_task(&session.id, "start").unwrap();
    drop(kernel);
    let mut replacement = preset();
    replacement.args.push("new-authority".into());
    let mut kernel = Kernel::open(database, Box::new(StartProvider)).unwrap();
    let service = register_config(
        &mut kernel,
        config(&directory.0, replacement),
        std::env::current_exe().unwrap(),
    )
    .unwrap();
    let result = kernel
        .respond_approval(
            &pending.id,
            &pending.pending_approval.unwrap().id,
            ApprovalDecision::AllowOnce,
        )
        .unwrap();
    assert_eq!(result.status, TaskStatus::Failed);
    assert!(result.output.unwrap().contains("no longer registered"));
    assert!(
        runtime()
            .unwrap()
            .block_on(service.list(&session.id))
            .unwrap()
            .is_empty()
    );
}

#[test]
fn native_worker_refusal_is_inspectable_and_reserved_creation_is_never_relaunched() {
    let directory = Directory::new();
    let worker = directory.0.join("invalid-worker.exe");
    fs::write(&worker, b"not a native executable").unwrap();
    let service = Service::new(directory.0.join("terminals"), worker).unwrap();
    let preset = Preset::resolve(preset(), &service.root).unwrap();
    let workspace = fliuno_protocol::Workspace {
        id: "workspace".into(),
        root: directory.0.to_string_lossy().into_owned(),
    };
    let context = ToolContext {
        task_id: "task",
        session_id: "session",
        workspace: &workspace,
    };
    let options = || CreateOptions {
        args: vec![],
        rows: 24,
        cols: 80,
    };
    let rt = runtime().unwrap();
    let failure = rt
        .block_on(service.create(
            &preset,
            &context,
            "create",
            options(),
            &CancellationToken::new(),
        ))
        .unwrap_err();
    assert!(matches!(failure, ToolError::Failed(_)));
    let listed = rt.block_on(service.list("session")).unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].status, Status::Failed);
    assert!(!listed[0].supervisor_connected);
    assert!(
        listed[0]
            .startup_error
            .as_ref()
            .unwrap()
            .contains("no PTY child started")
    );
    let failure = rt
        .block_on(service.create(
            &preset,
            &context,
            "create",
            options(),
            &CancellationToken::new(),
        ))
        .unwrap_err();
    let ToolError::OutcomeUnknownWithOutput { output, .. } = failure else {
        panic!("reserved ID lost")
    };
    assert_eq!(output["terminalId"], listed[0].terminal_id);
    assert_eq!(rt.block_on(service.list("session")).unwrap().len(), 1);
    assert!(
        rt.block_on(service.list("other-session"))
            .unwrap()
            .is_empty()
    );
}
