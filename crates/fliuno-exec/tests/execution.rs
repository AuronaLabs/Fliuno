//! Real programs, pipes, process-tree lifetime and durable kernel outcomes.
#![cfg(feature = "test-fixtures")]
#[cfg(any(windows, target_os = "linux"))]
mod assets;
use fliuno_core::{
    ApprovalOutcome, CancellationToken, DemoProvider, Kernel, ModelDecision, ModelProvider,
    ModelTurn, ToolEffect, ToolError, ToolSpec,
};
use fliuno_exec::{Config, ProcessIsolation, ProgramConfig, register_config};
use fliuno_protocol::{ApprovalDecision, EventKind, Message, Session, Task, TaskStatus, ToolCall};
use serde_json::{Value, json};
use std::{
    fs,
    path::PathBuf,
    sync::atomic::{AtomicUsize, Ordering},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

static NEXT_DIRECTORY: AtomicUsize = AtomicUsize::new(0);

struct Directory(PathBuf);
impl Directory {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "fliuno-exec-{}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed)
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
            return Ok(ModelDecision::Complete("Observed process result".into()).into());
        }
        let tool = tools
            .iter()
            .find(|spec| matches!(spec.effect, ToolEffect::ProcessExecution))
            .unwrap();
        Ok(ModelDecision::Call(ToolCall {
            id: format!("call_{}", task.id),
            name: tool.name.clone(),
            arguments: self.0.clone(),
        })
        .into())
    }
}
fn config(isolation: ProcessIsolation) -> ProgramConfig {
    ProgramConfig {
        assets: vec![],
        id: "probe".into(),
        command: env!("CARGO_BIN_EXE_fliuno-exec-probe").into(),
        isolation,
        resources: Default::default(),
        linux_cgroup_parent: None,
        args: vec![],
        env_allow: vec![],
        // Positive native IO probes include profile setup/staging and process
        // startup. Captured cold starts exceeded 8 s on this Windows host; the
        // separate 200/500 ms cases verify actual deadline behavior.
        timeout_ms: 30_000,
        output_bytes: 32 * 1024,
    }
}
fn setup(directory: &Directory, arguments: Value, program: ProgramConfig) -> (Kernel, Session) {
    let mut kernel = Kernel::in_memory(Box::new(Provider(arguments))).unwrap();
    register_config(
        &mut kernel,
        Config {
            programs: vec![program],
        },
    )
    .unwrap();
    let workspace = kernel
        .open_workspace(directory.0.to_str().unwrap())
        .unwrap();
    let session = kernel.create_session(&workspace.id).unwrap();
    (kernel, session)
}
fn approve(kernel: &mut Kernel, session: &Session) -> Task {
    let task = kernel.start_task(&session.id, "execute once").unwrap();
    assert_eq!(task.status, TaskStatus::WaitingApproval);
    kernel
        .respond_approval(
            &task.id,
            &task.pending_approval.unwrap().id,
            ApprovalDecision::AllowOnce,
        )
        .unwrap()
}
fn output(task: &Task) -> &Value {
    task.messages
        .iter()
        .find_map(|message| match message {
            Message::ToolResult { output, .. } => Some(output),
            _ => None,
        })
        .unwrap()
}
fn assert_complete(kernel: &Kernel, task: &Task, mode: ProcessIsolation) {
    assert_eq!(
        task.status,
        TaskStatus::Completed,
        "native fixture did not complete in {mode:?}; task: {task:?}; events: {:?}",
        kernel.list_events(&task.id, 0, 100).unwrap().events
    );
}
#[cfg(any(windows, target_os = "linux"))]
#[test]
fn trusted_execution_applies_the_approved_process_limit() {
    let directory = Directory::new();
    let args = json!({"args":["child-status"]});
    let mut limited = config(ProcessIsolation::TrustedHost);
    #[cfg(target_os = "linux")]
    {
        let Some(parent) = std::env::var_os("FLIUNO_EXEC_TEST_CGROUP_PARENT") else {
            eprintln!("set FLIUNO_EXEC_TEST_CGROUP_PARENT for delegated Linux validation");
            return;
        };
        limited.linux_cgroup_parent = Some(parent.into());
    }
    limited.resources.pids_max = Some(1);
    let baseline = config(ProcessIsolation::TrustedHost);
    for (program, spawned) in [(baseline, true), (limited, false)] {
        let (mut kernel, session) = setup(&directory, args.clone(), program);
        let task = approve(&mut kernel, &session);
        assert_complete(&kernel, &task, ProcessIsolation::TrustedHost);
        let report: Value =
            serde_json::from_str(output(&task)["stdout"]["data"].as_str().unwrap()).unwrap();
        assert_eq!(report["spawned"], spawned);
    }
}

fn modes() -> Vec<ProcessIsolation> {
    let mut modes = vec![ProcessIsolation::TrustedHost];
    if cfg!(windows) {
        modes.push(ProcessIsolation::WindowsAppContainer);
    }
    if cfg!(target_os = "linux") {
        modes.push(ProcessIsolation::LinuxLandlock);
    }
    modes
}

#[test]
fn approved_program_receives_literal_arguments_closed_stdin_and_separate_streams() {
    let directory = Directory::new();
    for mode in modes() {
        let arguments = json!({"args":["echo","你好","a b","x\"y","trailing\\","$(echo forbidden)","; echo forbidden"],"stdin":"多行\ninput\n"});
        let (mut kernel, session) = setup(&directory, arguments.clone(), config(mode));
        let task = approve(&mut kernel, &session);
        assert_complete(&kernel, &task, mode);
        let report = output(&task);
        assert_eq!(report["termination"], "exited");
        assert_eq!(report["exitCode"], 0);
        assert_eq!(report["success"], true);
        assert_eq!(report["evidence"]["stopCause"], "root_exited");
        for field in ["rootReaped", "stdinClosed", "stdoutClosed", "stderrClosed"] {
            assert_eq!(report["evidence"][field], true, "missing {field}: {report}");
        }
        let probe: Value =
            serde_json::from_str(report["stdout"]["data"].as_str().unwrap()).unwrap();
        assert_eq!(probe["stdin"], arguments["stdin"]);
        assert_eq!(
            probe["args"],
            json!([
                "你好",
                "a b",
                "x\"y",
                "trailing\\",
                "$(echo forbidden)",
                "; echo forbidden"
            ])
        );
        assert_eq!(probe["secret"], false);
        assert_eq!(
            probe["container"],
            mode == ProcessIsolation::WindowsAppContainer
        );
        if mode == ProcessIsolation::TrustedHost {
            assert_eq!(
                fs::canonicalize(probe["cwd"].as_str().unwrap()).unwrap(),
                fs::canonicalize(&directory.0).unwrap()
            );
        }
        assert!(
            report["stderr"]["data"]
                .as_str()
                .unwrap()
                .contains("stderr:中文")
        );
        assert!(!directory.0.join("forbidden").exists());
    }
}

#[test]
fn output_flood_is_drained_with_bounded_tails_and_binary_bytes_are_preserved() {
    let directory = Directory::new();
    for mode in modes() {
        let (mut kernel, session) = setup(
            &directory,
            json!({"args":["flood"],"outputBytes":1024}),
            config(mode),
        );
        let task = approve(&mut kernel, &session);
        assert_complete(&kernel, &task, mode);
        let report = output(&task);
        assert_eq!(report["success"], true, "{report}");
        for stream in ["stdout", "stderr"] {
            assert_eq!(report[stream]["retainedBytes"], 1024);
            assert_eq!(report[stream]["truncated"], true);
            assert!(report[stream]["bytesSeen"].as_u64().unwrap() >= 1024 * 1024);
        }
        assert!(
            report["stdout"]["data"]
                .as_str()
                .unwrap()
                .contains("tail-你好")
        );
        let (mut kernel, session) = setup(&directory, json!({"args":["binary"]}), config(mode));
        let task = approve(&mut kernel, &session);
        assert_complete(&kernel, &task, mode);
        let report = output(&task);
        assert_eq!(report["stdout"]["encoding"], "base64");
        assert_eq!(report["stdout"]["data"], "AP/+Cg==");
    }
}

#[test]
fn concurrent_native_workers_keep_lifetimes_and_streams_independent() {
    // Release from the parent after preparing every call. Workers never wait on
    // one another, so a preparation failure cannot deadlock an N-party barrier.
    let mut workers = Vec::new();
    for mode in modes() {
        for fixture in ["echo", "flood"] {
            let directory = Directory::new();
            let identity = format!("{mode:?}-{fixture}");
            let arguments = if fixture == "echo" {
                json!({"args":[fixture,identity],"stdin":format!("input-{identity}"),"outputBytes":1024})
            } else {
                json!({"args":[fixture],"outputBytes":1024})
            };
            let (mut kernel, session) = setup(&directory, arguments, config(mode));
            let task = kernel.start_task(&session.id, "concurrent IO").unwrap();
            let ApprovalOutcome::Prepared { execution, .. } = kernel
                .prepare_approval(
                    &task.id,
                    &task.pending_approval.unwrap().id,
                    ApprovalDecision::AllowOnce,
                )
                .unwrap()
            else {
                panic!("concurrent command was not prepared")
            };
            assert!(kernel.begin_tool_execution(&execution).unwrap());
            let (release, ready) = std::sync::mpsc::sync_channel(1);
            let worker = thread::spawn(move || {
                ready.recv_timeout(Duration::from_secs(30)).unwrap();
                let response = execution.execute(&CancellationToken::new());
                (execution, response)
            });
            workers.push((directory, kernel, release, worker, mode, fixture, identity));
        }
    }
    for (_, _, release, _, _, _, _) in &workers {
        release.send(()).unwrap();
    }
    for (_directory, mut kernel, _, worker, mode, fixture, identity) in workers {
        let (execution, response) = worker.join().unwrap();
        let task = kernel.apply_tool_response(&execution, response).unwrap();
        assert_eq!(
            task.status,
            TaskStatus::Running,
            "worker did not return a known result in {mode:?}: {task:?}; events: {:?}",
            kernel.list_events(&task.id, 0, 100).unwrap().events
        );
        let report = output(&task);
        assert_eq!(report["success"], true, "{mode:?}: {report}");
        assert_eq!(report["evidence"]["stopCause"], "root_exited");
        assert_eq!(report["evidence"]["rootReaped"], true);
        assert_eq!(report["evidence"]["cleanupExpired"], false);
        // A reaped Unix process group may already be absent. Its failed
        // terminate observation is distinct from an input/output failure.
        #[cfg(any(windows, target_os = "linux"))]
        assert!(
            report["evidence"]["faults"].as_array().unwrap().is_empty(),
            "{report}"
        );
        #[cfg(all(unix, not(target_os = "linux")))]
        for fault in report["evidence"]["faults"].as_array().unwrap() {
            assert_eq!(fault["operation"], "terminate", "{report}");
            assert_eq!(fault["osCode"], libc::ESRCH, "{report}");
        }
        if fixture == "echo" {
            let probe: Value =
                serde_json::from_str(report["stdout"]["data"].as_str().unwrap()).unwrap();
            assert_eq!(probe["args"], json!([identity]));
            assert_eq!(probe["stdin"], format!("input-{identity}"));
            assert_eq!(
                report["stderr"]["data"].as_str().unwrap().trim(),
                "stderr:中文"
            );
        } else {
            for stream in ["stdout", "stderr"] {
                assert_eq!(report[stream]["retainedBytes"], 1024);
                assert_eq!(report[stream]["truncated"], true);
                assert!(report[stream]["bytesSeen"].as_u64().unwrap() >= 1024 * 1024);
            }
            assert!(
                report["stdout"]["data"]
                    .as_str()
                    .unwrap()
                    .ends_with("tail-你好\n")
            );
            assert!(
                report["stderr"]["data"]
                    .as_str()
                    .unwrap()
                    .bytes()
                    .all(|b| b == b'e')
            );
        }
    }
}

#[test]
fn nonzero_exit_is_a_known_result_and_timeout_keeps_partial_evidence_without_retry() {
    let directory = Directory::new();
    let (mut kernel, session) = setup(
        &directory,
        json!({"args":["exit"]}),
        config(ProcessIsolation::TrustedHost),
    );
    let task = approve(&mut kernel, &session);
    assert_eq!(task.status, TaskStatus::Completed);
    assert_eq!(output(&task)["exitCode"], 7);
    assert_eq!(output(&task)["success"], false);
    let (mut kernel, session) = setup(
        &directory,
        json!({"args":["hang"],"timeoutMs":500}),
        config(ProcessIsolation::TrustedHost),
    );
    let task = approve(&mut kernel, &session);
    assert_eq!(task.status, TaskStatus::Interrupted);
    assert!(task.in_flight_tool.is_some());
    let events = kernel.list_events(&task.id, 0, 100).unwrap().events;
    let report = events
        .iter()
        .find_map(|event| match &event.data {
            EventKind::ToolOutcomeUnknown {
                output: Some(output),
                ..
            } => Some(output),
            _ => None,
        })
        .unwrap();
    assert_eq!(report["termination"], "timed_out");
    assert_eq!(report["evidence"]["stopCause"], "deadline");
    assert_eq!(report["evidence"]["rootReaped"], true);
    if report["evidence"]["launchMs"].as_u64().unwrap() >= 500 {
        assert_eq!(report["evidence"]["stdinSkipped"], true);
        assert_eq!(report["stdinComplete"], false);
    }
    assert!(report["stdout"]["data"].as_str().unwrap().contains("ready"));
    assert!(kernel.resume_task_with_options(&task.id, true).is_err());
    assert!(
        kernel
            .session_context(&session.id, Default::default())
            .unwrap()
            .turns
            .is_empty()
    );
}

#[test]
fn unconsumed_stdin_can_be_cancelled_and_cancelled_tasks_reject_late_results() {
    let directory = Directory::new();
    let marker = directory.0.join("started");
    let program = config(ProcessIsolation::TrustedHost);
    let startup_budget = Duration::from_millis(program.timeout_ms) + Duration::from_secs(2);
    let (mut kernel, session) = setup(
        &directory,
        json!({"args":["hang-file",marker],"stdin":"x".repeat(256*1024)}),
        program,
    );
    let task = kernel
        .start_task(&session.id, "cancel running command")
        .unwrap();
    let ApprovalOutcome::Prepared { execution, .. } = kernel
        .prepare_approval(
            &task.id,
            &task.pending_approval.unwrap().id,
            ApprovalDecision::AllowOnce,
        )
        .unwrap()
    else {
        panic!("not prepared")
    };
    assert!(kernel.begin_tool_execution(&execution).unwrap());
    let cancellation = CancellationToken::new();
    let worker_cancel = cancellation.clone();
    let worker = thread::spawn(move || {
        let response = execution.execute(&worker_cancel);
        (execution, response)
    });
    let deadline = Instant::now() + startup_budget;
    while !marker.exists() {
        if worker.is_finished() || Instant::now() >= deadline {
            cancellation.cancel();
            let (_, response) = worker.join().unwrap();
            match response {
                Err(error) => panic!("process did not reach the cancellation checkpoint: {error}"),
                Ok(_) => panic!("process exited successfully before the cancellation checkpoint"),
            }
        }
        thread::sleep(Duration::from_millis(10));
    }
    kernel.cancel_task(&task.id).unwrap();
    cancellation.cancel();
    let (execution, response) = worker.join().unwrap();
    let Err(ToolError::OutcomeUnknownWithOutput { output, .. }) = &response else {
        panic!("missing partial cancellation result")
    };
    assert_eq!(output["termination"], "cancelled");
    assert_eq!(output["evidence"]["stopCause"], "cancelled");
    assert!(output["stdout"]["data"].as_str().unwrap().contains("ready"));
    // OS pipe buffering varies by platform. Accepted writes do not prove that
    // the program consumed stdin; cancellation must work in either case.
    assert!(output["stdinComplete"].is_boolean());
    let cancelled = kernel.apply_tool_response(&execution, response).unwrap();
    assert_eq!(cancelled.status, TaskStatus::Cancelled);
    let events = kernel.list_events(&task.id, 0, 100).unwrap().events;
    assert!(events.iter().any(|event|matches!(&event.data,EventKind::ToolOutcomeUnknown{output:Some(output),..} if output["termination"]=="cancelled")));
    assert!(
        !events
            .iter()
            .any(|event| matches!(event.data, EventKind::ToolCompleted { .. }))
    );
}

#[test]
fn timeout_and_root_exit_both_reap_owned_descendants() {
    let directory = Directory::new();
    for mode in ["tree", "orphan"] {
        let marker = directory.0.join(mode);
        // Natural root exit is a positive native-startup path. A 200 ms
        // deadline is tested only where interruption is the expected result.
        let timeout = if mode == "tree" { 200 } else { 30_000 };
        let (mut kernel, session) = setup(
            &directory,
            json!({"args":[mode,marker],"timeoutMs":timeout}),
            config(ProcessIsolation::TrustedHost),
        );
        let task = approve(&mut kernel, &session);
        assert_eq!(
            task.status,
            if mode == "tree" {
                TaskStatus::Interrupted
            } else {
                TaskStatus::Completed
            },
            "{task:?}; events: {:?}",
            kernel.list_events(&task.id, 0, 100).unwrap().events
        );
    }
    thread::sleep(Duration::from_millis(1000));
    assert!(!directory.0.join("tree").exists());
    assert!(!directory.0.join("orphan").exists());
}

#[cfg(any(windows, target_os = "linux"))]
#[test]
fn timeout_reaps_a_descendant_confirmed_running_before_the_deadline() {
    let directory = Directory::new();
    let (mut kernel, session) = setup(
        &directory,
        json!({"args":["tree-hold","unused"],"timeoutMs":30000}),
        config(ProcessIsolation::TrustedHost),
    );
    let task = approve(&mut kernel, &session);
    assert_eq!(task.status, TaskStatus::Interrupted, "{task:?}");
    let events = kernel.list_events(&task.id, 0, 100).unwrap().events;
    let report = events
        .iter()
        .find_map(|event| match &event.data {
            EventKind::ToolOutcomeUnknown {
                output: Some(output),
                ..
            } => Some(output),
            _ => None,
        })
        .unwrap();
    assert_eq!(report["termination"], "timed_out", "{report}");
    assert_eq!(report["evidence"]["stopCause"], "deadline");
    assert_eq!(report["evidence"]["rootReaped"], true);
    assert_eq!(report["evidence"]["stdoutClosed"], true);
    let stdout = report["stdout"]["data"].as_str().unwrap();
    assert!(
        stdout.lines().any(|line| line == "descendant-ready"),
        "{report}"
    );
    let pid: u32 = stdout
        .lines()
        .find_map(|line| line.strip_prefix("descendant:"))
        .unwrap()
        .parse()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while descendant_is_running(pid) {
        assert!(
            Instant::now() < deadline,
            "owned descendant {pid} survived: {report}"
        );
        thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(windows)]
fn descendant_is_running(pid: u32) -> bool {
    fliuno_sandbox::process_is_running(pid).unwrap()
}
#[cfg(target_os = "linux")]
fn descendant_is_running(pid: u32) -> bool {
    // Read-only diagnostic of the fixture PID, never an authority to signal it.
    // A reparented zombie is no longer executing, even before init reaps it.
    match fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(stat) => !matches!(
            stat.rsplit_once(')').unwrap().1.split_whitespace().next(),
            Some("Z" | "X")
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => panic!("cannot observe fixture descendant: {error}"),
    }
}

#[test]
fn invalid_configuration_is_atomic_and_models_cannot_replace_authority_or_limits() {
    let mut kernel = Kernel::in_memory(Box::new(DemoProvider)).unwrap();
    let baseline = kernel.tool_specs().len();
    let program = config(ProcessIsolation::TrustedHost);
    let mut invalid = program.clone();
    invalid.id = "bad id".into();
    assert!(
        register_config(
            &mut kernel,
            Config {
                programs: vec![program.clone(), invalid]
            }
        )
        .is_err()
    );
    assert_eq!(kernel.tool_specs().len(), baseline);
    assert!(
        serde_json::from_value::<Config>(
            json!({"programs":[{"id":"missing-isolation","command":program.command}]})
        )
        .is_err()
    );
    for field in ["command", "cwd", "env", "isolation"] {
        let directory = Directory::new();
        let mut arguments = json!({"args":["echo"]});
        arguments[field] = json!("replacement");
        let (mut kernel, session) = setup(&directory, arguments, program.clone());
        assert_eq!(
            kernel
                .start_task(&session.id, "invalid authority")
                .unwrap()
                .status,
            TaskStatus::Failed
        );
    }
    for arguments in [
        json!({"timeoutMs":30001}),
        json!({"outputBytes":65537}),
        json!({"args":["\u{0000}"]}),
        json!({"stdin":"x".repeat(256*1024+1)}),
    ] {
        let directory = Directory::new();
        let (mut kernel, session) = setup(&directory, arguments, program.clone());
        assert_eq!(
            kernel
                .start_task(&session.id, "invalid budget")
                .unwrap()
                .status,
            TaskStatus::Failed
        );
    }
}

#[test]
fn executable_changes_and_configuration_changes_cannot_reuse_old_approval() {
    let directory = Directory::new();
    let binary = directory.0.join(if cfg!(windows) {
        "program.exe"
    } else {
        "program"
    });
    fs::copy(env!("CARGO_BIN_EXE_fliuno-exec-probe"), &binary).unwrap();
    let mut program = config(ProcessIsolation::TrustedHost);
    program.command = binary.clone();
    let (mut kernel, session) = setup(&directory, json!({"args":["echo"]}), program);
    let task = kernel.start_task(&session.id, "binary identity").unwrap();
    use std::io::Write;
    fs::OpenOptions::new()
        .append(true)
        .open(&binary)
        .unwrap()
        .write_all(b"changed")
        .unwrap();
    let failed = kernel
        .respond_approval(
            &task.id,
            &task.pending_approval.unwrap().id,
            ApprovalDecision::AllowOnce,
        )
        .unwrap();
    assert_eq!(failed.status, TaskStatus::Failed);
    assert!(failed.output.unwrap().contains("executable changed"));
    drop(kernel);
    let database = directory.0.join("kernel.db");
    let marker = directory.0.join("must-not-run");
    let mut kernel = Kernel::open(&database, Box::new(Provider(json!({})))).unwrap();
    let mut program = config(ProcessIsolation::TrustedHost);
    program.args = vec!["touch".into(), marker.to_string_lossy().into_owned()];
    register_config(
        &mut kernel,
        Config {
            programs: vec![program.clone()],
        },
    )
    .unwrap();
    let workspace = kernel
        .open_workspace(directory.0.to_str().unwrap())
        .unwrap();
    let session = kernel.create_session(&workspace.id).unwrap();
    let task = kernel
        .start_task(&session.id, "pending old configuration")
        .unwrap();
    drop(kernel);
    program.timeout_ms = 4000;
    let mut kernel = Kernel::open(&database, Box::new(Provider(json!({})))).unwrap();
    register_config(
        &mut kernel,
        Config {
            programs: vec![program],
        },
    )
    .unwrap();
    let failed = kernel
        .respond_approval(
            &task.id,
            &task.pending_approval.unwrap().id,
            ApprovalDecision::AllowOnce,
        )
        .unwrap();
    assert_eq!(failed.status, TaskStatus::Failed);
    assert!(!marker.exists());
}
