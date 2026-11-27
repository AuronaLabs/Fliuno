use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};
use std::{
    io::{Read, Write},
    path::{Path, PathBuf},
    process::Command,
    sync::mpsc::{Receiver, channel},
    thread,
    time::{Duration, Instant},
};

fn binary() -> PathBuf {
    std::env::var_os("FLIUNO_TEST_TUI")
        .map(PathBuf::from)
        .unwrap_or_else(|| env!("CARGO_BIN_EXE_fliuno").into())
}

fn size(rows: u16, cols: u16) -> PtySize {
    PtySize {
        rows,
        cols,
        pixel_width: 0,
        pixel_height: 0,
    }
}

struct Console {
    master: Box<dyn MasterPty + Send>,
    child: Box<dyn Child + Send + Sync>,
    input: Box<dyn Write + Send>,
    output: Receiver<Vec<u8>>,
    screen: vt100::Parser,
}

impl Console {
    fn launch(server: Option<&Path>, root: &Path) -> Self {
        Self::launch_with_config(server, root, None)
    }

    fn launch_with_config(server: Option<&Path>, root: &Path, config: Option<&Path>) -> Self {
        let mut command = CommandBuilder::new(binary());
        if let Some(config) = config {
            command.arg("--config");
            command.arg(config);
        } else {
            command.arg("--demo");
            command.arg("--db");
            command.arg(root.join("workbench.db"));
        }
        Self::spawn(command, server, root)
    }

    fn launch_unconfigured(server: Option<&Path>, root: &Path) -> Self {
        Self::spawn(CommandBuilder::new(binary()), server, root)
    }

    fn spawn(mut command: CommandBuilder, server: Option<&Path>, root: &Path) -> Self {
        let pair = native_pty_system().openpty(size(28, 110)).unwrap();
        if let Some(server) = server {
            command.arg("--server");
            command.arg(server);
        }
        command.arg("--workspace");
        command.arg(root);
        command.env("TERM", "xterm-256color");
        command.env("HOME", root);
        command.env("USERPROFILE", root);
        if let Some(theme) = std::env::var_os("FLIUNO_TEST_THEME") {
            command.arg("--theme");
            command.arg(theme);
        }
        let child = pair.slave.spawn_command(command).unwrap();
        drop(pair.slave);
        let mut reader = pair.master.try_clone_reader().unwrap();
        let input = pair.master.take_writer().unwrap();
        let (sender, output) = channel();
        thread::spawn(move || {
            let mut bytes = [0; 4096];
            while let Ok(n) = reader.read(&mut bytes) {
                if n == 0 || sender.send(bytes[..n].to_vec()).is_err() {
                    break;
                }
            }
        });
        Self {
            child,
            master: pair.master,
            input,
            output,
            screen: vt100::Parser::new(28, 110, 0),
        }
    }

    fn expect(&mut self, expected: &str) {
        self.wait_for(expected, true);
    }

    fn expect_absent(&mut self, expected: &str) {
        self.wait_for(expected, false);
    }

    fn wait_for(&mut self, expected: &str, present: bool) {
        let deadline = Instant::now() + Duration::from_secs(20);
        let mut transcript = Vec::new();
        while self.screen.screen().contents().contains(expected) != present {
            match self
                .output
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            {
                Ok(bytes) => {
                    self.observe(&bytes);
                    if transcript.len() < 4096 {
                        transcript.extend(&bytes);
                    }
                }
                Err(error) => panic!(
                    "expected {expected:?} (present: {present}): {error}\nchild: {:?}\nscreen: {}\noutput: {:?}",
                    self.child.try_wait().unwrap(),
                    self.screen.screen().contents(),
                    String::from_utf8_lossy(&transcript),
                ),
            }
        }
    }

    fn observe(&mut self, bytes: &[u8]) {
        self.screen.process(bytes);
        if bytes.windows(4).any(|command| command == b"\x1b[6n") {
            self.keys("\x1b[1;1R");
        }
    }

    fn keys(&mut self, keys: &str) {
        self.input.write_all(keys.as_bytes()).unwrap();
        self.input.flush().unwrap();
    }

    fn capture(&mut self, name: &str) {
        if let Some(directory) = std::env::var_os("FLIUNO_TUI_CAPTURE") {
            self.ready();
            let screen = self.screen.screen();
            let (rows, cols) = screen.size();
            let color = |color| match color {
                vt100::Color::Default => serde_json::Value::Null,
                vt100::Color::Idx(index) => serde_json::json!(index),
                vt100::Color::Rgb(r, g, b) => serde_json::json!([r, g, b]),
            };
            let cells = (0..rows)
                .map(|row| {
                    (0..cols)
                        .map(|col| {
                            let cell = screen.cell(row, col).expect("cell within viewport");
                            serde_json::json!([
                                cell.contents(),
                                color(cell.fgcolor()),
                                color(cell.bgcolor()),
                                cell.bold(),
                                cell.inverse(),
                                cell.is_wide_continuation(),
                            ])
                        })
                        .collect::<Vec<_>>()
                })
                .collect::<Vec<_>>();
            let directory = PathBuf::from(directory);
            std::fs::write(
                directory.join(format!("{name}.ansi")),
                screen.contents_formatted(),
            )
            .unwrap();
            std::fs::write(
                directory.join(format!("{name}.json")),
                serde_json::to_vec(&cells).unwrap(),
            )
            .unwrap();
        }
    }

    fn ready(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            match self.output.recv_timeout(Duration::from_millis(140)) {
                Ok(bytes) => self.observe(&bytes),
                Err(_) if !self.screen.screen().contents().contains("Working") => return,
                Err(_) => {}
            }
            assert!(Instant::now() < deadline, "workbench never became ready");
        }
    }

    fn resize(&mut self, rows: u16, cols: u16) {
        self.screen.set_size(rows, cols);
        self.master.resize(size(rows, cols)).unwrap();
    }

    fn quit(&mut self, key: &str) {
        self.keys(key);
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                assert!(status.success(), "{status:?}");
                break;
            }
            if let Ok(bytes) = self.output.recv_timeout(Duration::from_millis(25)) {
                self.observe(&bytes);
            }
            assert!(
                Instant::now() < deadline,
                "workbench did not exit with {key:?}:\n{}",
                self.screen.screen().contents()
            );
        }
        while let Ok(bytes) = self.output.recv_timeout(Duration::from_millis(100)) {
            self.screen.process(&bytes);
        }
        assert!(
            !self.screen.screen().alternate_screen(),
            "alternate screen still active"
        );
    }
}

impl Drop for Console {
    fn drop(&mut self) {
        if matches!(self.child.try_wait(), Ok(None)) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

#[test]
fn help_version_and_rejection_without_terminal() {
    let bin = binary();
    let help = Command::new(&bin).arg("--help").output().unwrap();
    assert!(help.status.success());
    assert!(String::from_utf8_lossy(&help.stdout).contains("local AI workbench"));
    let version = Command::new(&bin).arg("--version").output().unwrap();
    assert_eq!(
        String::from_utf8_lossy(&version.stdout).trim(),
        concat!("fliuno ", env!("CARGO_PKG_VERSION"))
    );
    let root = tempfile::tempdir().unwrap();
    let db = root.path().join("missing").join("db.sqlite");
    let result = Command::new(&bin).arg("--db").arg(&db).output().unwrap();
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("interactive terminal"));
    assert!(!db.parent().unwrap().exists());
}

#[cfg(target_os = "linux")]
#[test]
#[ignore = "requires a Linux PTY and /bin/sh"]
fn stalled_initialization_cannot_hold_the_terminal_open() {
    use std::os::unix::fs::PermissionsExt;

    let root = tempfile::tempdir().unwrap();
    let server = root.path().join("stalled-server");
    std::fs::write(
        &server,
        b"#!/bin/sh\nIFS= read -r request || exit 1\n: > \"$2.started\"\nIFS= read -r next\n",
    )
    .unwrap();
    std::fs::set_permissions(&server, std::fs::Permissions::from_mode(0o700)).unwrap();
    let mut console = Console::launch(Some(&server), root.path());
    console.expect("Connecting");
    let until = Instant::now() + Duration::from_secs(10);
    while !root.path().join("workbench.db.started").exists() {
        assert!(Instant::now() < until, "no initialize request");
        thread::sleep(Duration::from_millis(20));
    }
    console.quit("q");
}

#[test]
#[ignore = "requires a matching app-server beside fliuno or at FLIUNO_TEST_APP_SERVER"]
fn terminal_plan_mode_completes_without_creating_an_artifact() {
    let server = std::env::var_os("FLIUNO_TEST_APP_SERVER").map(PathBuf::from);
    let root = tempfile::tempdir().unwrap();
    let mut console = Console::launch(server.as_deref(), root.path());
    console.expect("New session");
    console.keys("Planning session\r");
    console.expect("No tasks");
    console.ready();
    console.keys("n");
    console.expect("New task");
    console.keys("\x1bOQ");
    console.expect("Plan only");
    console.keys("Plan the report\x13");
    console.expect("Demo plan ready for review.");
    console.expect("Completed");
    console.ready();
    assert!(
        !console
            .screen
            .screen()
            .contents()
            .contains("artifact_create")
    );
    console.quit("\x03");
}

#[test]
#[ignore = "requires a matching app-server beside fliuno or at FLIUNO_TEST_APP_SERVER"]
fn terminal_plan_queue_branch_and_timeline_workflow() {
    let server = std::env::var_os("FLIUNO_TEST_APP_SERVER").map(PathBuf::from);
    let root = tempfile::tempdir().unwrap();
    let mut console = Console::launch(server.as_deref(), root.path());
    console.expect("New session");
    console.keys("Planning desk\r");
    console.expect("No tasks");
    console.ready();
    console.keys("n");
    console.expect("New task");
    console.keys("\x1bOQ");
    console.expect("Plan only");
    console.keys("Draft a release plan\x05");
    console.expect("Prompt queued");
    console.ready();
    console.quit("q");

    let mut console = Console::launch(server.as_deref(), root.path());
    console.expect("Planning desk");
    console.ready();
    console.keys("Q");
    console.expect("Queued prompts (1)");
    console.expect("Draft a release plan");
    console.capture("prompt-queue");
    console.ready();
    console.keys("\r");
    console.expect("Demo plan ready for review.");
    console.expect("Completed");
    console.capture("plan-completed");
    console.ready();
    console.keys("t");
    console.expect("Task timeline");
    console.expect("task started");
    console.capture("timeline");
    console.ready();
    console.keys("\r");
    console.expect("task_started");
    console.ready();
    console.keys("\x1b");
    console.expect("Task timeline");
    console.keys("\x1b");
    console.expect_absent("Task timeline");
    console.keys("c");
    console.expect("Context and memory usage");
    console.expect("history turns");
    console.capture("context");
    console.ready();
    console.keys("\x1b");
    console.expect_absent("Context and memory usage");
    console.expect("Demo plan ready for review.");
    console.keys("B");
    console.expect("Name the session branch");
    console.keys("Release alternative\r");
    console.expect("Release alternative");
    console.expect("1 inherited turns");
    console.ready();
    console.keys("s");
    console.expect("Sessions");
    console.ready();
    console.keys("f");
    console.expect("Find sessions by title or ID");
    console.keys("Planning desk\r");
    console.expect("Sessions  /  Planning desk");
    console.ready();
    console.keys("\r");
    console.expect("Completed");
    console.ready();
    console.keys("P");
    console.expect("Implement the following plan");
    console.expect("Execution");
    console.keys("\x13");
    console.expect("Review operation");
    console.quit("\x03");
}

#[test]
#[ignore = "requires a matching app-server beside fliuno or at FLIUNO_TEST_APP_SERVER"]
fn terminal_workspace_inbox_and_history_search_cross_sessions() {
    let server = std::env::var_os("FLIUNO_TEST_APP_SERVER").map(PathBuf::from);
    let root = tempfile::tempdir().unwrap();
    let mut console = Console::launch(server.as_deref(), root.path());
    console.expect("New session");
    console.keys("First session\r");
    console.expect("First session");
    console.expect("No tasks");
    console.ready();

    console.keys("n");
    console.expect("New task");
    console.keys("Cross session approval\x13");
    console.expect("Review operation");
    console.expect("artifact_create");
    console.ready();

    console.keys("s");
    console.expect("Sessions");
    console.ready();
    console.keys("n");
    console.expect("New session");
    console.keys("Second session\r");
    console.expect("Second session");
    console.expect("No tasks");
    console.ready();

    console.keys("I");
    console.expect("Workspace inbox");
    console.expect("Cross session approval");
    console.keys("\r");
    console.expect("y Allow once");
    console.expect("n Deny");
    console.expect("artifact_create");
    console.ready();
    console.keys("y");
    console.expect("Completed");
    console.ready();

    console.keys("H");
    console.expect("Search workspace history");
    console.keys("Cross session approval\r");
    console.expect("· all · seq");
    console.expect("Completed  ·  Cross session approval");
    console.ready();
    console.keys("a");
    console.expect("· assistant · seq");
    console.expect("No matching history.");
    console.ready();
    console.keys("u");
    console.expect("· user · seq");
    console.expect("Completed  ·  Cross session approval");
    console.ready();
    console.keys("\r");
    console.expect("Task detail");
    console.expect("First session");
    console.expect("Completed");
    console.quit("\x03");
}

#[test]
#[ignore = "requires a matching app-server beside fliuno or at FLIUNO_TEST_APP_SERVER"]
fn task_browser_can_open_a_prompt_outside_the_recent_list() {
    use fliuno_client::Client;
    use serde_json::{Value, json};
    use std::{io::BufReader, process::Stdio};

    let server = std::env::var_os("FLIUNO_TEST_APP_SERVER")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            binary().with_file_name(if cfg!(windows) {
                "fliuno-app-server.exe"
            } else {
                "fliuno-app-server"
            })
        });
    let root = tempfile::tempdir().unwrap();
    let mut child = Command::new(&server)
        .arg("--db")
        .arg(root.path().join("workbench.db"))
        .env("FLIUNO_PROVIDER", "demo")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut client = Client::connect(
        BufReader::new(child.stdout.take().unwrap()),
        child.stdin.take().unwrap(),
        "terminal-browser-test",
        |_| {},
    )
    .unwrap();
    let workspace: Value = client
        .call("workspace/open", &json!({"root":root.path()}), |_| {})
        .unwrap();
    let session: Value = client
        .call(
            "session/create",
            &json!({"workspaceId":workspace["id"],"title":"工作任务历史"}),
            |_| {},
        )
        .unwrap();
    for n in 0..41 {
        let prompt = if n == 0 {
            "Original prompt".to_owned()
        } else {
            format!("Newer prompt {n:02}")
        };
        let task: Value = client
            .call(
                "task/startSummary",
                &json!({"sessionId":session["id"], "input":prompt}),
                |_| {},
            )
            .unwrap();
        let _: Value = client
            .call("task/cancel", &json!({"taskId":task["id"]}), |_| {})
            .unwrap();
    }
    let (mut output, input) = client.into_parts();
    drop(input);
    std::io::copy(&mut output, &mut std::io::sink()).unwrap();
    assert!(child.wait().unwrap().success());
    let mut console = Console::launch(Some(&server), root.path());
    console.expect("工作任务历史");
    console.expect("Newer prompt 40");
    console.ready();
    console.keys("L");
    console.expect("Page 1");
    console.expect("Newer prompt 40");
    console.capture("task-browser-many");
    console.ready();
    console.keys("\x1b[6~");
    console.expect("Page 2");
    console.expect("Newer prompt 20");
    console.ready();
    console.keys("\x1b[5~");
    console.expect("Page 1");
    console.ready();
    console.keys("\x1b[6~");
    console.expect("Page 2");
    console.ready();
    console.keys("\x1b[6~");
    console.expect("Page 3");
    console.expect("Original prompt");
    console.ready();
    console.resize(18, 48);
    console.expect("Enter Open");
    console.expect("r Refresh");
    console.capture("task-browser-narrow");
    console.ready();
    console.keys("\r");
    console.expect_absent("Session tasks");
    console.expect("Original prompt");
    console.quit("\x03");
}

#[test]
#[ignore = "requires a matching app-server beside fliuno or at FLIUNO_TEST_APP_SERVER"]
fn terminal_demo_reviews_and_reopens_one_artifact() {
    let server = std::env::var_os("FLIUNO_TEST_APP_SERVER").map(PathBuf::from);
    let root = tempfile::tempdir().unwrap();
    let mut console = Console::launch(server.as_deref(), root.path());
    console.expect("New session");
    console.keys("Acceptance\r");
    console.expect("Acceptance");
    console.expect("No tasks");
    console.ready();
    console.capture("empty-session");
    console.keys("/");
    console.expect("Commands  /");
    console.capture("command-menu");
    console.keys("new task\r");
    console.expect("New task");
    if cfg!(windows) {
        console.keys("交付报告\rsecond line");
    } else {
        console.keys("\x1b[200~交付报告\nsecond line\x1b[201~");
    }
    console.expect("second line");
    console.capture("draft");
    console.keys("\x13");
    console.expect("Review operation");
    console.ready();
    console.keys("a");
    console.expect("y Allow once");
    console.expect("n Deny");
    console.expect("artifact_create");
    console.keys("y");
    console.expect("Completed");
    console.ready();
    console.keys("L");
    console.expect("All states");
    console.expect("second line");
    console.capture("task-browser-wide");
    console.keys("f");
    console.expect("No tasks match this filter.");
    console.ready();
    console.keys("\x1b");
    console.expect_absent("Session tasks");
    console.expect("v Open");
    console.capture("conversation-completed");
    console.keys("v");
    console.expect("text/plain");
    console.keys("\r");
    console.expect("SHA-256:");
    console.expect("交付报告");
    console.capture("artifact");
    let saved = root.path().join("verified.txt");
    console.keys("d");
    console.expect("Save artifact to");
    console.keys(&format!("{}\r", saved.display()));
    console.expect("Saved ");
    assert_eq!(
        std::fs::read_to_string(&saved).unwrap(),
        "交付报告\nsecond line"
    );
    console.ready();
    console.resize(18, 48);
    console.expect("Shift+L Tasks");
    console.expect("交付报告");
    console.capture("conversation-narrow");
    console.keys("\t");
    console.expect("Task detail");
    console.ready();
    assert!(!console.screen.screen().contents().contains("Session tasks"));
    console.quit("\x03");
    let mut reopened = Console::launch(server.as_deref(), root.path());
    reopened.expect("Acceptance");
    reopened.expect("Completed");
    reopened.ready();
    reopened.keys("n");
    reopened.expect("New task");
    reopened.keys("Reject this draft\x13");
    reopened.expect("Review operation");
    reopened.ready();
    reopened.resize(18, 48);
    reopened.expect("Shift+L Tasks");
    reopened.keys("a");
    reopened.expect("y Allow once");
    reopened.expect("n Deny");
    reopened.expect("artifact_create");
    reopened.capture("narrow-approval");
    reopened.keys("n");
    reopened.expect("Failed");
    reopened.expect("Failure reason");
    reopened.expect("tool execution denied");
    reopened.ready();
    reopened.resize(28, 110);
    reopened.expect("Shift+L All tasks");
    reopened.keys("n");
    reopened.expect("New task");
    reopened.keys("Cancel this draft\x13");
    reopened.expect("Review operation");
    reopened.ready();
    reopened.keys("x");
    reopened.expect("Cancel task?");
    reopened.keys("y");
    reopened.expect("Cancelled");
    reopened.quit("q");
}

#[test]
#[ignore = "requires a matching app-server beside fliuno or at FLIUNO_TEST_APP_SERVER"]
fn draft_context_controls_survive_queue_restart() {
    use fliuno_client::Client;
    use serde_json::{Value, json};
    use std::{io::BufReader, process::Stdio};

    let server = std::env::var_os("FLIUNO_TEST_APP_SERVER").map(PathBuf::from);
    let root = tempfile::tempdir().unwrap();
    let mut console = Console::launch(server.as_deref(), root.path());
    console.expect("New session");
    console.keys("Context desk\r");
    console.expect("No tasks");
    console.ready();
    console.keys("n");
    console.expect("New task");
    console.keys("\x1bOQ");
    console.expect("Plan only");
    console.keys("Review the work history\x13");
    console.expect("Demo plan ready for review.");
    console.expect("Completed");
    console.ready();
    console.keys("n");
    console.keys("Use the reviewed context");
    console.keys("\x1bOR");
    console.expect("Draft context");
    console.expect("1 completed turns");
    console.expect("Automatic memory is selected");
    console.ready();
    console.capture("draft-context-wide");
    console.keys("s");
    console.expect("Isolated prompt");
    console.expect("without session history");
    console.ready();
    console.resize(18, 48);
    console.expect("Esc Return to draft");
    console.expect("Isolated prompt");
    console.capture("draft-context-isolated");
    console.keys("s");
    console.expect("1 completed turns");
    console.ready();
    console.keys("m");
    console.expect("Memory   Off");
    console.ready();
    console.keys("h");
    console.expect("Conversation only");
    console.ready();
    console.keys("p");
    console.expect("Pinned at #");
    console.ready();
    console.capture("draft-context-pinned");
    console.keys("\x1b");
    console.expect("History pinned");
    console.expect("Use the reviewed context");
    console.capture("draft-context-composer");
    console.keys("\x05");
    console.expect("Prompt queued");
    console.ready();
    console.quit("q");

    let mut console = Console::launch(server.as_deref(), root.path());
    console.expect("Context desk");
    console.ready();
    console.keys("Q");
    console.expect("Queued prompts (1)");
    console.expect("Use the reviewed context");
    console.ready();
    console.keys("\r");
    console.expect("Demo plan ready for review.");
    console.expect("Completed");
    console.ready();
    console.keys("c");
    console.expect("Context and memory usage");
    console.expect("history turns");
    console.capture("draft-context-attribution");
    console.quit("\x03");

    let server = server.unwrap_or_else(|| {
        binary().with_file_name(if cfg!(windows) {
            "fliuno-app-server.exe"
        } else {
            "fliuno-app-server"
        })
    });
    let mut child = Command::new(server)
        .arg("--db")
        .arg(root.path().join("workbench.db"))
        .env("FLIUNO_PROVIDER", "demo")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut client = Client::connect(
        BufReader::new(child.stdout.take().unwrap()),
        child.stdin.take().unwrap(),
        "terminal-context-test",
        |_| {},
    )
    .unwrap();
    let workspace: Value = client
        .call("workspace/open", &json!({"root":root.path()}), |_| {})
        .unwrap();
    let sessions: Value = client
        .call(
            "session/list",
            &json!({"workspaceId":workspace["id"]}),
            |_| {},
        )
        .unwrap();
    let tasks: Value = client
        .call(
            "task/listSummaries",
            &json!({"sessionId":sessions["items"][0]["id"]}),
            |_| {},
        )
        .unwrap();
    let queued = tasks["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|task| task["inputPreview"] == "Use the reviewed context")
        .unwrap();
    let task: Value = client
        .call("task/get", &json!({"taskId":queued["id"]}), |_| {})
        .unwrap();
    let context = &task["context"];
    assert_eq!(context["options"]["mode"], "session");
    assert_eq!(context["options"]["detail"], "conversation");
    assert_eq!(context["options"]["disableMemory"], true);
    assert_eq!(context["options"]["planOnly"], true);
    assert!(context["throughSequence"].as_u64().unwrap() > 0);
    assert_eq!(
        context["throughSequence"],
        context["options"]["throughSequence"]
    );
    assert_eq!(context["turns"].as_array().unwrap().len(), 1);
    assert!(context.get("memories").is_none());
    let (mut output, input) = client.into_parts();
    drop(input);
    std::io::copy(&mut output, &mut std::io::sink()).unwrap();
    assert!(child.wait().unwrap().success());
}

#[test]
#[ignore = "requires a matching app-server beside fliuno or at FLIUNO_TEST_APP_SERVER"]
fn terminal_uses_user_configuration_and_relative_state() {
    let server = std::env::var_os("FLIUNO_TEST_APP_SERVER").map(PathBuf::from);
    let root = tempfile::tempdir().unwrap();
    let config = root.path().join("settings/config.toml");
    let init = || {
        Command::new(binary())
            .args(["--init-config", "--config"])
            .arg(&config)
            .output()
            .unwrap()
    };
    assert!(init().status.success());
    let template = std::fs::read_to_string(&config).unwrap();
    assert!(!init().status.success());
    assert_eq!(std::fs::read_to_string(&config).unwrap(), template);
    std::fs::write(
        &config,
        r#"
        [workbench]
        theme = "dark"
        database = "state/configured.db"
        [model]
        provider = "demo"
    "#,
    )
    .unwrap();
    let mut console = Console::launch_with_config(server.as_deref(), root.path(), Some(&config));
    console.expect("New session");
    console.keys("Configured workbench\r");
    console.expect("No tasks yet");
    console.ready();
    let database = root.path().join("settings/state/configured.db");
    assert!(database.is_file());
    assert!(!root.path().join("workbench.db").exists());
    console.resize(24, 80);
    console.expect("Shift+L All tasks");
    console.ready();
    console.capture("configured-80-columns");
    console.quit("q");
    let mut reopened = Console::launch_with_config(server.as_deref(), root.path(), Some(&config));
    reopened.expect("Configured workbench");
    reopened.ready();
    reopened.quit("q");
}

#[test]
#[ignore = "requires a matching app-server beside fliuno or at FLIUNO_TEST_APP_SERVER"]
fn model_and_budget_load_from_files_without_provider_environment() {
    use fliuno_client::Client;
    use std::{io::BufReader, process::Stdio};

    let server = std::env::var_os("FLIUNO_TEST_APP_SERVER")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            binary().with_file_name(if cfg!(windows) {
                "fliuno-app-server.exe"
            } else {
                "fliuno-app-server"
            })
        });
    let root = tempfile::tempdir().unwrap();
    let config = root.path().join("config.toml");
    std::fs::write(root.path().join("fixture.key"), "offline-test-key\n").unwrap();
    std::fs::write(
        &config,
        r#"
        [workbench]
        database = "terminal-model.db"
        [model]
        provider = "openai"
        name = "fixture-model"
        api_key_file = "fixture.key"
        base_url = "http://127.0.0.1:9/v1/"
        [model.budget]
        window = 8192
        max_output_tokens = 1024
    "#,
    )
    .unwrap();
    let mut process = Command::new(&server)
        .arg("--config")
        .arg(&config)
        .arg("--db")
        .arg(root.path().join("model.db"))
        .env_remove("OPENAI_API_KEY")
        .env_remove("FLIUNO_MODEL")
        .env("FLIUNO_PROVIDER", "legacy-env-should-not-take-precedence")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let client = Client::connect(
        BufReader::new(process.stdout.take().unwrap()),
        process.stdin.take().unwrap(),
        "terminal-config-test",
        |_| {},
    )
    .unwrap();
    let capabilities = &client.info().capabilities;
    assert_eq!(capabilities["provider"], "openai");
    assert_eq!(capabilities["modelBudget"]["maxOutputTokens"], 1024);
    let (mut reader, writer) = client.into_parts();
    drop(writer);
    std::io::copy(&mut reader, &mut std::io::sink()).unwrap();
    assert!(process.wait().unwrap().success());
    let mut terminal = Console::launch_with_config(Some(&server), root.path(), Some(&config));
    terminal.expect("Start a conversation");
    terminal.expect("Message Fliuno");
    terminal.ready();
    terminal.keys("\x1bOP");
    terminal.expect("Connection and configuration");
    terminal.keys("connection\r");
    terminal.expect("Model: openai / fixture-model");
    terminal.expect("Active provider: openai");
    terminal.expect("Service: Connected");
    terminal.capture("diagnostics-live-model");
    terminal.resize(20, 48);
    terminal.ready();
    terminal.expect("fixture-model");
    terminal.expect("Service: Connected");
    terminal.capture("diagnostics-narrow");
    terminal.quit("\x03");
}

#[test]
fn doctor_checks_local_settings_without_starting_or_disclosing_credentials() {
    let root = tempfile::tempdir().unwrap();
    let config = root.path().join("config.toml");
    let data = r#"
        [workbench]
        database = "new/workbench.db"
        server = "missing-server"
        [model]
        provider = "openai"
        name = "fixture-model"
        api_key_file = "fixture.key"
    "#;
    std::fs::write(&config, data).unwrap();
    let diagnose = || {
        Command::new(binary())
            .arg("--doctor")
            .arg("--config")
            .arg(&config)
            .arg("--workspace")
            .arg(root.path())
            .output()
            .unwrap()
    };
    let missing = diagnose();
    assert!(!missing.status.success());
    let stdout = String::from_utf8(missing.stdout).unwrap();
    assert!(
        stdout.contains("Cannot read the configured API key file"),
        "{stdout}"
    );
    assert!(stdout.contains("missing-server"), "{stdout}");
    assert!(stdout.contains("missing or invalid"), "{stdout}");
    assert!(!root.path().join("new").exists());
    let fliuno = serde_json::to_string(&binary().to_string_lossy()).unwrap();
    std::fs::write(&config, data.replace("\"missing-server\"", &fliuno)).unwrap();
    std::fs::write(root.path().join("fixture.key"), "must-never-be-printed\n").unwrap();
    let valid = diagnose();
    let stdout = String::from_utf8(valid.stdout).unwrap();
    assert!(valid.status.success(), "{stdout}");
    assert!(
        stdout.contains("Configured key file is readable"),
        "{stdout}"
    );
    assert!(!stdout.contains("must-never-be-printed"));
    assert!(!root.path().join("new").exists());
    std::fs::write(&config, "[model").unwrap();
    let invalid = diagnose();
    assert!(!invalid.status.success());
    assert!(String::from_utf8_lossy(&invalid.stderr).contains("Invalid configuration"));
}

#[test]
#[ignore = "requires a matching app-server beside fliuno or at FLIUNO_TEST_APP_SERVER"]
fn terminal_creates_verifies_and_opens_a_restored_database() {
    let server = std::env::var_os("FLIUNO_TEST_APP_SERVER").map(PathBuf::from);
    let root = tempfile::tempdir().unwrap();
    let copy = tempfile::tempdir().unwrap();
    let backup = root.path().join("backup copy");
    let restored = copy.path().join("workbench.db");
    let mut console = Console::launch(server.as_deref(), root.path());
    console.expect("New session");
    console.keys("Restore acceptance\r");
    console.expect("No tasks");
    console.ready();
    console.keys("n");
    console.keys("Approval and deliverable in the snapshot\x13");
    console.expect("Review operation");
    console.ready();
    console.keys("a");
    console.expect("y Allow once");
    console.keys("y");
    console.expect("Completed");
    console.ready();
    console.keys("/");
    console.expect("Database backups");
    console.keys("database backups\r");
    console.expect("c Create");
    console.capture("backup-menu-wide");
    console.keys("c");
    console.expect("Create backup in a new folder");
    console.keys(&format!("{}\r", backup.display()));
    console.expect("Backup created");
    console.expect("Tasks: 1");
    console.ready();
    console.keys("v");
    console.expect("Verify backup folder");
    console.keys(&format!("{}\r", backup.display()));
    console.expect("Backup verified");
    console.ready();
    console.keys("r");
    console.expect("Restore from backup folder");
    console.keys(&format!("{}\r", backup.display()));
    console.expect("Restore to a new database path");
    console.keys(&format!("{}\r", restored.display()));
    console.expect("Database restored");
    console.expect("SHA-256");
    console.ready();
    console.resize(18, 48);
    console.ready();
    console.expect("Open this copy with fliuno --db PATH.");
    console.capture("backup-restored-narrow");
    console.keys("r");
    console.expect("Restore from backup folder");
    console.keys(&format!("{}\r", backup.display()));
    console.expect("Restore to a new database path");
    console.keys(&format!("{}\r", restored.display()));
    console.expect("exists;");
    console.quit("\x03");

    let config = root.path().join("restored.toml");
    let database = serde_json::to_string(&restored.to_string_lossy()).unwrap();
    std::fs::write(&config, format!("[workbench]\ndatabase = {database}\n")).unwrap();
    let mut console = Console::launch_with_config(server.as_deref(), root.path(), Some(&config));
    console.expect("Restore acceptance");
    console.expect("Approval and deliverable in the snapshot");
    console.expect("Completed");
    console.ready();
    console.keys("v");
    console.expect("text/plain");
    console.keys("\r");
    console.expect("SHA-256:");
    console.expect("Approval and deliverable in the snapshot");
    console.capture("backup-reopened-deliverable");
    console.quit("\x03");
}

#[test]
#[ignore = "requires a matching app-server beside fliuno or at FLIUNO_TEST_APP_SERVER"]
fn live_preview_follows_output_and_keeps_manual_reading_position() {
    use serde_json::json;
    use std::{
        io::{BufRead, BufReader},
        net::TcpListener,
    };

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let (advance, checkpoints) = channel();
    let provider = thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let mut input = BufReader::new(&mut socket);
        let mut line = String::new();
        let mut content_length = 0;
        loop {
            line.clear();
            assert!(input.read_line(&mut line).unwrap() > 0);
            if line == "\r\n" {
                break;
            }
            if let Some((header, value)) = line.split_once(':')
                && header.eq_ignore_ascii_case("Content-Length")
            {
                content_length = value.trim().parse().unwrap();
            }
        }
        let mut request = vec![0; content_length];
        input.read_exact(&mut request).unwrap();
        let request: serde_json::Value = serde_json::from_slice(&request).unwrap();
        assert_eq!(request["stream"], true);
        socket
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
            )
            .unwrap();
        let mut send = |event: serde_json::Value| {
            write!(socket, "data: {event}\n\n").unwrap();
            socket.flush().unwrap();
        };
        let first_lines = (1..=32)
            .map(|row| format!("Draft line {row}\n"))
            .collect::<String>();
        send(
            json!({"type":"response.output_text.delta","delta":format!("{first_lines}FIRST-LIVE-END")}),
        );
        checkpoints.recv_timeout(Duration::from_secs(20)).unwrap();
        send(json!({"type":"response.output_text.delta","delta":"\nSECOND-LIVE-END"}));
        checkpoints.recv_timeout(Duration::from_secs(20)).unwrap();
        let final_text = format!("{first_lines}PERSISTED-END");
        send(
            json!({"type":"response.completed","response":{"status":"completed","output":[{"type":"message","content":[{"type":"output_text","text":final_text}]}],"usage":{"input_tokens":12,"output_tokens":4}}}),
        );
    });
    let server = std::env::var_os("FLIUNO_TEST_APP_SERVER").map(PathBuf::from);
    let root = tempfile::tempdir().unwrap();
    let config = root.path().join("live.toml");
    std::fs::write(root.path().join("fixture.key"), "fixture-key\n").unwrap();
    std::fs::write(
        &config,
        format!(
            r#"
        [workbench]
        database = "live.db"
        [model]
        provider = "openai"
        name = "fixture-stream"
        api_key_file = "fixture.key"
        base_url = "http://{address}/v1/"
    "#
        ),
    )
    .unwrap();
    let mut terminal = Console::launch_with_config(server.as_deref(), root.path(), Some(&config));
    terminal.expect("Start a conversation");
    terminal.expect("Message Fliuno");
    terminal.ready();
    terminal.capture("conversation-first-message");
    terminal.keys("Live transcript start\r");
    terminal.expect("FIRST-LIVE-END");
    terminal.expect("Following");
    terminal.capture("activity-follow-live");
    terminal.keys("\x1b[H");
    terminal.expect("Live transcript start");
    terminal.expect_absent("Following");
    advance.send(()).unwrap();
    terminal.capture("activity-manual-reading");
    terminal.keys("\x1b[F");
    terminal.expect("SECOND-LIVE-END");
    advance.send(()).unwrap();
    terminal.expect("PERSISTED-END");
    terminal.expect("12 input");
    terminal.expect("4 output");
    terminal.expect("Completed");
    terminal.capture("activity-final-usage");
    terminal.quit("\x03");
    provider.join().unwrap();
}

#[test]
#[ignore = "requires a matching app-server beside fliuno or at FLIUNO_TEST_APP_SERVER"]
fn terminal_reviews_edits_and_pins_a_workspaces_memory() {
    let server = std::env::var_os("FLIUNO_TEST_APP_SERVER").map(PathBuf::from);
    let root = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let mut terminal = Console::launch(server.as_deref(), root.path());
    terminal.expect("New session");
    terminal.keys("Memory acceptance\r");
    terminal.expect("No tasks");
    terminal.ready();
    let open_memories = |terminal: &mut Console| {
        terminal.keys("/");
        terminal.expect("Workspace memories");
        terminal.keys("workspace memories\r");
        terminal.expect("Saved workspace memories");
        terminal.ready();
    };
    open_memories(&mut terminal);
    terminal.keys("n");
    terminal.expect("Memory key");
    terminal.keys("rust-notes\r");
    terminal.expect("New memory");
    terminal.keys("Rust 系统开发\x13");
    terminal.expect("rust-notes  ·  v1");
    terminal.ready();
    terminal.keys("f");
    terminal.expect("Find memories");
    terminal.keys("系统开发\r");
    terminal.expect("Results for 系统开发");
    terminal.expect("rust-notes  ·  v1");
    terminal.ready();
    terminal.keys("\r");
    terminal.expect("Source: Manual entry");
    terminal.expect("Rust 系统开发");
    terminal.capture("memory-source-wide");
    terminal.keys("e");
    terminal.expect("Editing rust-notes");
    terminal.keys(" for kernel work\x13");
    terminal.expect("rust-notes  ·  v2");
    terminal.ready();
    terminal.keys("f");
    terminal.expect("Find memories");
    if cfg!(windows) {
        terminal.keys("\x08\x08\x08\x08");
    } else {
        terminal.keys("\x7f\x7f\x7f\x7f");
    }
    terminal.keys("kernel\r");
    terminal.expect("Results for kernel");
    terminal.expect("rust-notes  ·  v2");
    terminal.ready();
    terminal.resize(20, 48);
    terminal.ready();
    terminal.capture("memory-filter-narrow");
    terminal.keys("\x1b");
    terminal.expect_absent("Workspace memories");
    terminal.resize(28, 110);
    terminal.ready();
    terminal.keys("n");
    terminal.expect("New task");
    terminal.keys("Review workspace design decisions\x1bOR");
    terminal.expect("Draft context");
    terminal.ready();
    terminal.keys("v");
    terminal.expect("0 of 8 selected");
    terminal.expect("rust-notes  ·  v2");
    terminal.ready();
    terminal.keys(" ");
    terminal.expect("1 of 8 selected");
    terminal.capture("memory-draft-selection");
    terminal.keys("\x13");
    terminal.expect("New task");
    terminal.keys("\x1bOR");
    terminal.expect("Pinned memory revisions: 1");
    terminal.keys("\x1b");
    terminal.expect("New task");
    terminal.keys("\x13");
    terminal.expect("Review operation");
    terminal.ready();
    terminal.keys("a");
    terminal.expect("y Allow once");
    terminal.keys("y");
    terminal.expect("Completed");
    terminal.ready();
    terminal.keys("c");
    terminal.expect("1 memories");
    terminal.expect("revision 2");
    terminal.keys("\x1b");
    terminal.expect_absent("Context and memory usage");
    terminal.ready();
    terminal.keys("w");
    terminal.expect("Workspaces");
    terminal.keys("o");
    terminal.expect("Open folder");
    terminal.keys(&format!("{}\r", outside.path().display()));
    terminal.expect("New session");
    terminal.keys("Separate workspace\r");
    terminal.expect("No tasks");
    terminal.ready();
    open_memories(&mut terminal);
    terminal.expect("No memories match");
    terminal.expect_absent("rust-notes");
    terminal.quit("\x03");
}

#[test]
#[ignore = "requires a matching app-server beside fliuno or at FLIUNO_TEST_APP_SERVER"]
fn workspace_deliverables_filter_review_versions_and_verify_saved_bytes() {
    use fliuno_client::Client;
    use serde_json::{Value, json};
    use std::{io::BufReader, process::Stdio};

    let server = std::env::var_os("FLIUNO_TEST_APP_SERVER")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            binary().with_file_name(format!("fliuno-app-server{}", std::env::consts::EXE_SUFFIX))
        });
    let root = tempfile::tempdir().unwrap();
    let mut child = Command::new(&server)
        .arg("--demo")
        .arg("--db")
        .arg(root.path().join("workbench.db"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut client = Client::connect(
        BufReader::new(child.stdout.take().unwrap()),
        child.stdin.take().unwrap(),
        "deliverable-catalog-test",
        |_| {},
    )
    .unwrap();
    let workspace: Value = client
        .call("workspace/open", &json!({"root":root.path()}), |_| {})
        .unwrap();
    let first: Value = client
        .call(
            "session/create",
            &json!({"workspaceId":workspace["id"],"title":"Revision work"}),
            |_| {},
        )
        .unwrap();
    let second: Value = client
        .call(
            "session/create",
            &json!({"workspaceId":workspace["id"],"title":"Another session"}),
            |_| {},
        )
        .unwrap();
    let mut create = |session: &Value, prompt: &str| {
        let task: Value = client
            .call(
                "task/start",
                &json!({"sessionId":session["id"],"input":prompt}),
                |_| {},
            )
            .unwrap();
        let target = json!({"taskId":task["id"]});
        let until = Instant::now() + Duration::from_secs(15);
        let approval = loop {
            let task: Value = client.call("task/get", &target, |_| {}).unwrap();
            if let Some(id) = task["pendingApproval"]["id"].as_str() {
                break id.to_owned();
            }
            assert!(Instant::now() < until, "no approval appeared");
            thread::sleep(Duration::from_millis(30));
        };
        let _: Value = client
            .call(
                "approval/respond",
                &json!({"taskId":task["id"],"approvalId":approval,"decision":"allow_once"}),
                |_| {},
            )
            .unwrap();
        let until = Instant::now() + Duration::from_secs(15);
        loop {
            let task: Value = client.call("task/get", &target, |_| {}).unwrap();
            if task["status"] == "completed" {
                break;
            }
            assert!(Instant::now() < until, "task did not finish");
            thread::sleep(Duration::from_millis(30));
        }
        let page: Value = client
            .call("artifacts/listSummaries", &target, |_| {})
            .unwrap();
        json!({"taskId":task["id"],"artifactId":page["items"][0]["id"]})
    };
    let previous = create(&first, "First version of the report");
    let current = create(&first, "Second version of the report");
    create(&second, "Another session deliverable");
    let _: Value = client
        .call(
            "artifacts/linkRevision",
            &json!({"previous":previous,"current":current}),
            |_| {},
        )
        .unwrap();
    let (mut output, input) = client.into_parts();
    drop(input);
    std::io::copy(&mut output, &mut std::io::sink()).unwrap();
    assert!(child.wait().unwrap().success());

    let mut console = Console::launch(Some(&server), root.path());
    console.expect("Another session");
    console.expect("Another session deliverable");
    console.ready();
    console.keys("/");
    console.expect("Workspace deliverables");
    console.keys("workspace deliverables\r");
    console.expect("all types · Page 1");
    console.expect("Demo artifact");
    console.ready();
    console.keys("f");
    console.expect("text/plain · Page 1");
    console.ready();
    console.capture("deliverables-filtered");
    console.keys("f");
    console.expect("text/markdown · Page 1");
    console.expect("No deliverables match");
    console.ready();
    console.keys("f");
    console.expect("application/json · Page 1");
    console.ready();
    console.keys("f");
    console.expect("application/pdf · Page 1");
    console.ready();
    console.keys("f");
    console.expect("all types · Page 1");
    console.ready();
    console.keys("\x1b[B\r");
    console.expect("Deliverable preview");
    console.expect("Second version of the report");
    console.expect("Earlier revision:");
    console.capture("deliverables-revision");
    console.keys("p");
    console.expect("First version of the report");
    console.ready();
    console.resize(21, 48);
    console.ready();
    console.capture("deliverables-narrow");
    console.resize(28, 110);
    console.ready();
    console.keys("d");
    console.expect("Destination file path");
    let destination = root.path().join("reviewed report.txt");
    std::fs::write(&destination, b"existing file").unwrap();
    console.keys(&format!("{}\r", destination.display()));
    console.expect("Destination already exists");
    assert_eq!(std::fs::read(&destination).unwrap(), b"existing file");
    console.ready();
    let output = root.path().join("saved report.txt");
    console.keys("d");
    console.expect("Destination file path");
    console.keys(&format!("{}\r", output.display()));
    console.expect("Verified SHA-256:");
    console.expect("saved report.txt");
    assert_eq!(
        std::fs::read(&output).unwrap(),
        b"First version of the report"
    );
    console.capture("deliverables-save-receipt");
    console.quit("\x03");
}

#[test]
#[ignore = "requires a matching app-server beside fliuno or at FLIUNO_TEST_APP_SERVER"]
fn first_launch_guides_live_setup_and_reopens_the_profile() {
    let server = std::env::var_os("FLIUNO_TEST_APP_SERVER").map(PathBuf::from);
    let root = tempfile::tempdir().unwrap();
    let profile = root.path().join(".fliuno");
    let config = profile.join("config.toml");
    let data = profile.join("state/workbench.db");
    let doctor = Command::new(binary())
        .arg("--doctor")
        .current_dir(root.path())
        .env("HOME", root.path())
        .env("USERPROFILE", root.path())
        .output()
        .unwrap();
    assert!(!doctor.status.success());
    assert!(String::from_utf8_lossy(&doctor.stdout).contains("No model profile"));
    assert!(!profile.exists());

    let mut cancelled = Console::launch_unconfigured(server.as_deref(), root.path());
    cancelled.expect("Choose your model provider");
    cancelled.expect("Google Gemini");
    cancelled.capture("setup-provider-110");
    cancelled.quit("\x03");
    assert!(
        !profile.exists(),
        "opening and cancelling must leave no profile"
    );

    let mut console = Console::launch_unconfigured(server.as_deref(), root.path());
    console.expect("Choose your model provider");
    console.keys("\r");
    console.expect("Enter a model ID");
    console.resize(24, 80);
    console.capture("setup-model-80");
    console.keys("fixture-model\r");
    console.expect("Connect an API key file");
    console.resize(21, 48);
    console.keys("fixture key.txt\r");
    console.expect("Cannot read the configured API key file");
    console.capture("setup-key-file-48");
    assert!(!config.exists());
    assert!(!data.exists());
    std::fs::create_dir_all(&profile).unwrap();
    std::fs::write(profile.join("fixture key.txt"), "fixture-secret-value\n").unwrap();
    std::fs::write(&config, "concurrent profile").unwrap();
    console.keys("\r");
    console.expect("A profile already exists.");
    assert_eq!(
        std::fs::read_to_string(&config).unwrap(),
        "concurrent profile"
    );
    std::fs::remove_file(&config).unwrap();
    console.keys("\r");
    console.expect("Start a conversation");
    console.expect("Message Fliuno");
    console.expect_absent("Offline demo");
    console.expect_absent("fixture-secret-value");
    console.capture("conversation-first-launch-48");
    let profile_text = std::fs::read_to_string(&config).unwrap();
    assert!(profile_text.contains("provider = \"openai\""));
    assert!(profile_text.contains("api_key_file = \"fixture key.txt\""));
    assert!(!profile_text.contains("fixture-secret-value"));
    assert!(data.is_file());
    console.quit("\x03");

    let mut reopened = Console::launch_unconfigured(server.as_deref(), root.path());
    reopened.expect("Start a conversation");
    reopened.expect("Message Fliuno");
    reopened.expect_absent("Choose your model provider");
    reopened.quit("\x03");
    assert_eq!(std::fs::read_to_string(config).unwrap(), profile_text);
}
