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
        Self::launch_with_config_at(server, root, config, (28, 110))
    }

    fn launch_with_config_at(
        server: Option<&Path>,
        root: &Path,
        config: Option<&Path>,
        viewport: (u16, u16),
    ) -> Self {
        let mut command = CommandBuilder::new(binary());
        if let Some(config) = config {
            command.arg("--config");
            command.arg(config);
        } else {
            command.arg("--demo");
            command.arg("--db");
            command.arg(root.join("workbench.db"));
        }
        Self::spawn_at(command, server, root, viewport)
    }

    fn launch_unconfigured(server: Option<&Path>, root: &Path) -> Self {
        Self::spawn(CommandBuilder::new(binary()), server, root)
    }

    fn spawn(command: CommandBuilder, server: Option<&Path>, root: &Path) -> Self {
        Self::spawn_at(command, server, root, (28, 110))
    }

    fn spawn_at(
        mut command: CommandBuilder,
        server: Option<&Path>,
        root: &Path,
        (rows, cols): (u16, u16),
    ) -> Self {
        let pair = native_pty_system().openpty(size(rows, cols)).unwrap();
        if let Some(server) = server {
            command.arg("--server");
            command.arg(server);
        }
        command.arg("--workspace");
        command.arg(root);
        command.env("TERM", "xterm-256color");
        command.env("HOME", root);
        command.env("USERPROFILE", root);
        command.env("LOCALAPPDATA", root.join("local"));
        command.env("XDG_STATE_HOME", root.join("state"));
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
            screen: vt100::Parser::new(rows, cols, 0),
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
                Err(_)
                    if !self.screen.screen().contents().contains("Working")
                        && !self
                            .screen
                            .screen()
                            .contents()
                            .contains("Opening conversation…") =>
                {
                    return;
                }
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
    assert_eq!(capabilities["modelCapabilities"]["model"], "fixture-model");
    assert_eq!(capabilities["modelBudget"]["maxOutputTokens"], 1024);
    let (mut reader, writer) = client.into_parts();
    drop(writer);
    std::io::copy(&mut reader, &mut std::io::sink()).unwrap();
    assert!(process.wait().unwrap().success());
    let mut terminal = Console::launch_with_config(Some(&server), root.path(), Some(&config));
    terminal.expect("Start a conversation");
    terminal.expect("Message Fliuno");
    terminal.expect("·  fixture-model");
    terminal.capture("active-model-110");
    terminal.resize(20, 48);
    terminal.ready();
    terminal.expect("·  fixture-model");
    terminal.capture("active-model-48");
    terminal.resize(30, 110);
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
#[ignore = "requires a matching app-server beside fliuno or at FLIUNO_TEST_APP_SERVER"]
fn named_model_profiles_select_the_actual_server_model_without_rewriting_config() {
    let server = std::env::var_os("FLIUNO_TEST_APP_SERVER").map(PathBuf::from);
    let root = tempfile::tempdir().unwrap();
    let config = root.path().join("config.toml");
    let settings = r#"
        [workbench]
        database = "profiles.db"
        model_profile = "office"
        [models.office]
        provider = "openai"
        name = "office-fixture"
        api_key_file = "profiles/office.key"
        base_url = "http://127.0.0.1:9/v1/"
        [models.lab]
        provider = "openai"
        name = "lab-fixture"
        api_key_file = "profiles/lab.key"
        base_url = "http://127.0.0.1:9/v1/"
    "#;
    std::fs::create_dir(root.path().join("profiles")).unwrap();
    std::fs::write(
        root.path().join("profiles/office.key"),
        "office-fixture-key\n",
    )
    .unwrap();
    std::fs::write(root.path().join("profiles/lab.key"), "lab-fixture-key\n").unwrap();
    std::fs::write(&config, settings).unwrap();

    let mut default = Console::launch_with_config(server.as_deref(), root.path(), Some(&config));
    default.expect("Start a conversation");
    default.expect("office-fixture");
    default.quit("\x03");

    let mut alternate = CommandBuilder::new(binary());
    alternate.arg("--config");
    alternate.arg(&config);
    alternate.args(["--model-profile", "lab"]);
    let mut terminal = Console::spawn(alternate, server.as_deref(), root.path());
    terminal.expect("Start a conversation");
    terminal.expect("lab-fixture");
    terminal.expect_absent("office-fixture");
    terminal.quit("\x03");

    let mut doctor = Command::new(binary());
    doctor
        .arg("--config")
        .arg(&config)
        .args(["--model-profile", "lab", "--doctor"]);
    if let Some(server) = &server {
        doctor.arg("--server").arg(server);
    }
    let report = doctor.output().unwrap();
    let summary = String::from_utf8(report.stdout).unwrap();
    assert!(report.status.success(), "{summary}");
    assert!(summary.contains("Model profile: lab"), "{summary}");
    assert!(summary.contains("Model: openai / lab-fixture"), "{summary}");
    assert!(!summary.contains("lab-fixture-key"));

    let unknown = Command::new(binary())
        .arg("--config")
        .arg(&config)
        .args(["--model-profile", "missing", "--doctor"])
        .output()
        .unwrap();
    assert!(!unknown.status.success());
    assert!(String::from_utf8_lossy(&unknown.stderr).contains("Unknown model profile missing"));
    assert_eq!(std::fs::read_to_string(&config).unwrap(), settings);
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

fn streaming_fixture_request(listener: &std::net::TcpListener) -> std::net::TcpStream {
    streaming_fixture_input(listener).0
}

fn streaming_fixture_input(
    listener: &std::net::TcpListener,
) -> (std::net::TcpStream, serde_json::Value) {
    use std::io::{BufRead, BufReader};

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
    (socket, request)
}

#[test]
#[ignore = "requires a matching app-server beside fliuno or at FLIUNO_TEST_APP_SERVER"]
fn live_preview_follows_output_and_keeps_manual_reading_position() {
    use serde_json::json;
    use std::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let (advance, checkpoints) = channel();
    let provider = thread::spawn(move || {
        let mut socket = streaming_fixture_request(&listener);
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
    terminal.keys("Live transcript start\x0esecond input line");
    terminal.expect("second input line");
    terminal.keys("\r");
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
    terminal.expect("Ask a follow-up");
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
    terminal.ready();
    terminal.keys("o");
    terminal.expect("Open folder");
    terminal.keys(&outside.path().to_string_lossy());
    terminal.expect(outside.path().file_name().unwrap().to_str().unwrap());
    terminal.keys("\r");
    terminal.expect_absent("Open folder");
    terminal.expect("New session");
    terminal.keys("Separate workspace\r");
    terminal.expect("Fliuno  /  Separate workspace");
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

fn read_catalog_request(
    listener: &std::net::TcpListener,
    key: Option<&str>,
) -> std::net::TcpStream {
    use std::io::{BufRead, BufReader};

    let (mut socket, _) = listener.accept().unwrap();
    socket
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let mut reader = BufReader::new(&mut socket);
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    assert!(line.starts_with("GET /v1/models HTTP/1.1"), "{line}");
    let mut authorization = None;
    loop {
        line.clear();
        assert!(reader.read_line(&mut line).unwrap() > 0);
        if line == "\r\n" {
            break;
        }
        if let Some((header, value)) = line.split_once(':')
            && header.eq_ignore_ascii_case("authorization")
        {
            assert!(authorization.replace(value.trim().to_owned()).is_none());
        }
    }
    assert_eq!(
        authorization.is_some(),
        key.is_some(),
        "unexpected authentication header"
    );
    if let Some(key) = key {
        assert!(
            authorization
                .as_deref()
                .and_then(|value| value.strip_prefix("Bearer "))
                == Some(key),
            "catalog request must use the selected credential"
        );
    }
    socket
}

fn write_catalog_response(mut socket: std::net::TcpStream, code: u16, body: &str) {
    write!(socket, "HTTP/1.1 {code} Test\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
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
        .env("LOCALAPPDATA", root.path().join("local"))
        .env("XDG_STATE_HOME", root.path().join("state"))
        .output()
        .unwrap();
    assert!(!doctor.status.success());
    assert!(String::from_utf8_lossy(&doctor.stdout).contains("No model profile"));
    assert!(!profile.exists());

    let mut cancelled = Console::launch_unconfigured(server.as_deref(), root.path());
    cancelled.resize(28, 110);
    cancelled.expect("Choose your model provider");
    cancelled.expect("Google Gemini Interactions");
    cancelled.capture("setup-provider-110");
    cancelled.resize(24, 80);
    cancelled.capture("setup-provider-80");
    cancelled.resize(21, 48);
    cancelled.capture("setup-provider-48");
    cancelled.quit("\x03");
    assert!(
        !profile.exists(),
        "opening and cancelling must leave no profile"
    );

    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}/v1/", listener.local_addr().unwrap());
    let (release_catalog, wait_for_filter) = channel();
    let model_service = thread::spawn(move || {
        let rejected = read_catalog_request(&listener, Some("fixture-rejected-key"));
        write_catalog_response(rejected, 401, r#"{"error":"fixture-rejected-key"}"#);
        let catalog = read_catalog_request(&listener, Some("fixture-secret-value"));
        wait_for_filter
            .recv_timeout(Duration::from_secs(5))
            .unwrap();
        write_catalog_response(
            catalog,
            200,
            r#"{"data":[{"id":"alpha-model"},{"id":"fixture-catalog-model"}]}"#,
        );
        let (mut stream, request) = streaming_fixture_input(&listener);
        assert_eq!(request["model"], "fixture-catalog-model");
        assert_eq!(request["reasoning_effort"], "high");
        stream.write_all(concat!(
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Connected from the catalog.\"},\"finish_reason\":null}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
            "data: [DONE]\n\n",
        ).as_bytes()).unwrap();
    });

    let mut console = Console::launch_unconfigured(server.as_deref(), root.path());
    console.expect("Choose your model provider");
    console.keys("\x1b[B\r");
    console.expect("Set a Base URL");
    console.resize(24, 80);
    console.capture("setup-base-url-80");
    console.keys("http://example.invalid/v1\r");
    console.expect("requires HTTPS");
    console.keys("\x1b[H");
    console.keys(&"\x1b[3~".repeat("http://example.invalid/v1".len()));
    console.keys(&format!("{base}\r"));
    console.expect("Connect your API key");
    console.keys("\x1bOQ");
    console.expect("Connect an API key file");
    console.resize(21, 48);
    console.keys("fixture key.txt\r");
    console.expect("Cannot read the configured API key file");
    console.capture("setup-key-file-48");
    assert!(!config.exists());
    assert!(!data.exists());
    std::fs::create_dir_all(&profile).unwrap();
    let key_file = profile.join("fixture key.txt");
    std::fs::write(&key_file, "fixture-rejected-key\n").unwrap();
    console.keys("\r");
    console.expect("API key rejected");
    console.expect_absent("fixture-rejected-key");
    console.capture("setup-auth-rejected-48");
    assert!(!config.exists());
    assert!(!data.exists());
    std::fs::write(key_file, "fixture-secret-value\n").unwrap();
    console.keys("\x1b[15~");
    console.expect("Loading the model catalog");
    console.keys("catalog");
    console.expect("› catalog");
    console.capture("setup-catalog-loading-48");
    release_catalog.send(()).unwrap();
    console.expect("fixture-catalog-model");
    console.expect_absent("alpha-model");
    console.capture("setup-catalog-result-48");
    console.resize(24, 80);
    console.capture("setup-catalog-result-80");
    console.resize(28, 110);
    console.capture("setup-catalog-result-110");
    console.keys("\t");
    console.expect("› fixture-catalog-model");

    console.keys("\x1bOR");
    console.expect("Set reasoning effort");
    console.expect("Provider default");
    console.keys(&"\x1b[B".repeat(5));
    console.resize(21, 48);
    console.capture("setup-reasoning-high-48");
    console.resize(24, 80);
    console.capture("setup-reasoning-high-80");
    console.resize(28, 110);
    console.capture("setup-reasoning-high-110");
    console.keys("\r");
    console.expect("F3 Reasoning: high");
    console.expect("› fixture-catalog-model");

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
    console.resize(21, 48);
    console.capture("conversation-first-launch-48");
    let profile_text = std::fs::read_to_string(&config).unwrap();
    assert!(profile_text.contains("provider = \"openai\""));
    assert!(profile_text.contains("protocol = \"chat_completions\""));
    assert!(profile_text.contains(&format!("base_url = \"{base}\"")));
    assert!(profile_text.contains("name = \"fixture-catalog-model\""));
    assert!(profile_text.contains("reasoning_effort = \"high\""));
    assert!(profile_text.contains("api_key_file = \"fixture key.txt\""));
    assert!(!profile_text.contains("fixture-secret-value"));
    assert!(data.is_file());
    console.keys("Use the catalog model.\r");
    console.expect("Connected from the catalog.");
    console.resize(28, 110);
    console.capture("setup-catalog-conversation-110");
    model_service.join().unwrap();
    console.quit("\x03");

    let mut reopened = Console::launch_unconfigured(server.as_deref(), root.path());
    reopened.expect("Message Fliuno");
    reopened.expect_absent("Choose your model provider");
    reopened.quit("\x03");
    assert_eq!(std::fs::read_to_string(config).unwrap(), profile_text);
}

#[test]
#[ignore = "requires a matching app-server beside fliuno or at FLIUNO_TEST_APP_SERVER"]
fn first_launch_guides_keyless_loopback_and_persists_the_profile() {
    let server = std::env::var_os("FLIUNO_TEST_APP_SERVER").map(PathBuf::from);
    let root = tempfile::tempdir().unwrap();
    let profile = root.path().join(".fliuno");
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}/v1/", listener.local_addr().unwrap());
    let model_service = thread::spawn(move || {
        let stream = read_catalog_request(&listener, None);
        write_catalog_response(stream, 200, r#"{"data":[{"id":"keyless-model"}]}"#);
        let (mut stream, request) = streaming_fixture_input(&listener);
        assert_eq!(request["model"], "keyless-model");
        stream.write_all(concat!(
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Keyless connection works.\"},\"finish_reason\":null}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
            "data: [DONE]\n\n",
        ).as_bytes()).unwrap();
    });
    let mut console = Console::launch_unconfigured(server.as_deref(), root.path());
    console.expect("Choose your model provider");
    console.keys("\x1b[B\r");
    console.expect("Set a Base URL");
    console.keys(&format!("{base}\r"));
    console.expect("F4 No key");
    console.keys("\x1bOS");
    console.expect("Connect without a key");
    for (rows, cols) in [(21, 48), (24, 80), (28, 110)] {
        console.resize(rows, cols);
        console.capture(&format!("setup-keyless-{cols}"));
    }
    console.keys("\r");
    console.expect("keyless-model");
    console.keys("\t");
    console.expect("› keyless-model");
    console.keys("\r");
    console.expect("Enter Send");
    console.keys("Hello local model\r");
    console.expect("Keyless connection works.");
    model_service.join().unwrap();
    console.quit("\x03");
    let config = std::fs::read_to_string(profile.join("config.toml")).unwrap();
    assert!(config.contains("auth = \"none\""));
    assert!(!config.contains("api_key"));
    assert!(!profile.join("credentials").exists());
    let mut reopened = Console::launch_unconfigured(server.as_deref(), root.path());
    reopened.expect("Keyless connection works.");
    reopened.expect_absent("Choose your model provider");
    reopened.quit("\x03");
}

#[test]
#[ignore = "requires a matching app-server beside fliuno or at FLIUNO_TEST_APP_SERVER"]
fn first_launch_stores_a_pasted_key_without_exposing_it() {
    let server = std::env::var_os("FLIUNO_TEST_APP_SERVER").map(PathBuf::from);
    let root = tempfile::tempdir().unwrap();
    let config = root.path().join(".fliuno/config.toml");
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}/v1/", listener.local_addr().unwrap());
    let catalog = thread::spawn(move || {
        let stream = read_catalog_request(&listener, Some("fixture-managed-secret-value"));
        write_catalog_response(stream, 200, r#"{"data":[]}"#);
    });
    let mut terminal = Console::launch_unconfigured(server.as_deref(), root.path());
    terminal.expect("Choose your model provider");
    terminal.keys("\r");
    terminal.expect("Set a Base URL");
    terminal.keys(&format!("{base}\r"));
    terminal.expect("Connect your API key");
    terminal.resize(21, 48);
    terminal.ready();
    terminal.keys("\r");
    terminal.expect("Paste your API key");
    terminal.keys("fixture-managed-secret-value");
    terminal.expect("••••••");
    terminal.expect_absent("fixture-managed-secret-value");
    terminal.capture("setup-managed-key-48");
    terminal.keys("\r");
    terminal.expect("The catalog is empty");
    terminal.expect_absent("fixture-managed-secret-value");
    assert!(!config.parent().unwrap().exists());
    terminal.capture("setup-catalog-empty-48");
    terminal.keys("fixture-managed\r");
    catalog.join().unwrap();

    let deadline = Instant::now() + Duration::from_secs(10);
    while !config.is_file() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(20));
    }
    let text = std::fs::read_to_string(&config).unwrap();
    let settings = fliuno_config::Settings::load(&config).unwrap();
    #[cfg(windows)]
    let _cleanup = {
        struct RemoveFixture(String);
        impl Drop for RemoveFixture {
            fn drop(&mut self) {
                let _ = Command::new("cmdkey")
                    .arg(format!("/delete:{}.fliuno-model", self.0))
                    .output();
            }
        }
        let id = settings.model.api_key_credential.as_ref().unwrap();
        assert!(uuid::Uuid::parse_str(id).is_ok());
        RemoveFixture(id.clone())
    };
    #[cfg(unix)]
    assert!(settings.model.api_key_file.as_ref().unwrap().is_file());
    assert!(!text.contains("fixture-managed-secret-value"));
    assert_eq!(
        settings.model.read_api_key().unwrap(),
        "fixture-managed-secret-value"
    );

    terminal.expect("Start a conversation");
    terminal.expect("fixture-managed");
    terminal.quit("\x03");
    let mut doctor = Command::new(binary());
    doctor
        .arg("--doctor")
        .arg("--config")
        .arg(&config)
        .arg("--workspace")
        .arg(root.path());
    if let Some(server) = &server {
        doctor.arg("--server").arg(server);
    }
    let diagnostic = doctor.output().unwrap();
    assert!(
        diagnostic.status.success(),
        "{}",
        String::from_utf8_lossy(&diagnostic.stdout)
    );
    assert!(!String::from_utf8_lossy(&diagnostic.stdout).contains("fixture-managed-secret-value"));

    let mut reopened = Console::launch_unconfigured(server.as_deref(), root.path());
    reopened.expect("Start a conversation");
    reopened.expect("fixture-managed");
    reopened.expect_absent("Connect your API key");
    reopened.quit("\x03");
}

#[test]
#[ignore = "requires a matching app-server beside fliuno or at FLIUNO_TEST_APP_SERVER"]
fn conversation_keeps_order_loads_older_turns_and_reopens_with_live_responses() {
    use fliuno_client::Client;
    use serde_json::{Value, json};
    use std::{io::BufReader, net::TcpListener, process::Stdio};

    let root = tempfile::tempdir().unwrap();
    let server = std::env::var_os("FLIUNO_TEST_APP_SERVER")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            binary().with_file_name(format!("fliuno-app-server{}", std::env::consts::EXE_SUFFIX))
        });
    let database = root.path().join("live.db");
    let mut child = Command::new(&server)
        .arg("--db")
        .arg(&database)
        .arg("--demo")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut client = Client::connect(
        BufReader::new(child.stdout.take().unwrap()),
        child.stdin.take().unwrap(),
        "conversation-acceptance",
        |_| {},
    )
    .unwrap();
    let workspace: Value = client
        .call("workspace/open", &json!({"root":root.path()}), |_| {})
        .unwrap();
    let session: Value = client
        .call(
            "session/create",
            &json!({"workspaceId":workspace["id"],"title":"Release discussion"}),
            |_| {},
        )
        .unwrap();
    for index in 1..=15 {
        let mut input = format!("Earlier prompt {index:02}");
        if index == 1 {
            input.push_str(&" The discussion includes a longer paragraph for reflow.".repeat(9));
        }
        let task: Value = client
            .call(
                "task/start",
                &json!({"sessionId":session["id"],"input":input,"context":{"planOnly":true}}),
                |_| {},
            )
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let current: Value = client
                .call("task/get", &json!({"taskId":task["id"]}), |_| {})
                .unwrap();
            if current["status"] == "completed" {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "seed turn did not complete: {current}"
            );
            thread::sleep(Duration::from_millis(20));
        }
    }
    let (mut output, input) = client.into_parts();
    drop(input);
    std::io::copy(&mut output, &mut std::io::sink()).unwrap();
    assert!(child.wait().unwrap().success());

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let provider = thread::spawn(move || {
        for text in ["First live answer", "Second live answer"] {
            let mut socket = streaming_fixture_request(&listener);
            let event = json!({"type":"response.completed","response":{"status":"completed","output":[{"type":"message","content":[{"type":"output_text","text":text}]}]}});
            write!(socket, "data: {event}\n\n").unwrap();
        }
    });
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

    let mut terminal = Console::launch_with_config(Some(&server), root.path(), Some(&config));
    terminal.expect("Earlier prompt 15");
    terminal.ready();
    terminal.keys("\x1b[H");
    terminal.expect("Earlier prompt 04");
    terminal.expect("PgUp History");
    terminal.expect_absent("Earlier prompt 01");
    terminal.keys("\x1b[5~");
    terminal.ready();
    terminal.expect("Earlier prompt 04");
    terminal.expect_absent("Earlier prompt 01");
    terminal.keys("\x1b[H");
    terminal.expect("Earlier prompt 01");
    let top_line = |console: &Console| {
        let screen = console.screen.screen();
        screen
            .rows(0, screen.size().1)
            .nth(3)
            .unwrap()
            .trim()
            .to_owned()
    };
    for _ in 0..24 {
        if top_line(&terminal) == "Earlier prompt 02" {
            break;
        }
        terminal.keys("\x1b[B");
        terminal.ready();
    }
    assert_eq!(top_line(&terminal), "Earlier prompt 02");
    terminal.capture("conversation-anchor-110");
    terminal.resize(22, 48);
    terminal.ready();
    assert_eq!(top_line(&terminal), "Earlier prompt 02");
    terminal.capture("conversation-anchor-48");
    terminal.resize(28, 110);
    terminal.ready();
    assert_eq!(top_line(&terminal), "Earlier prompt 02");
    terminal.keys("\x1b[F");
    terminal.expect("Earlier prompt 15");
    terminal.keys("\r");
    terminal.expect("Enter Send");
    terminal.keys("First live question");
    terminal.expect("First live question");
    terminal.keys("\r");
    terminal.expect("First live answer");
    terminal.expect("Enter Send");
    terminal.ready();
    terminal.keys("Second live question");
    terminal.expect("Second live question");
    terminal.keys("\r");
    terminal.expect("Second live answer");
    terminal.ready();
    let assert_order = |terminal: &Console| {
        let content = terminal.screen.screen().contents();
        let ordered = [
            "First live question",
            "First live answer",
            "Second live question",
            "Second live answer",
        ];
        assert!(
            ordered
                .windows(2)
                .all(|pair| content.find(pair[0]).unwrap() < content.find(pair[1]).unwrap()),
            "{content}"
        );
        for item in ordered {
            assert_eq!(content.matches(item).count(), 1, "{content}");
        }
    };
    assert_order(&terminal);
    terminal.capture("conversation-two-live-turns-110");
    terminal.resize(16, 80);
    terminal.expect("Second live answer");
    terminal.capture("conversation-follow-up-80");
    terminal.quit("\x03");
    provider.join().unwrap();

    let mut reopened = Console::launch_with_config(Some(&server), root.path(), Some(&config));
    reopened.expect("Second live answer");
    reopened.ready();
    assert_order(&reopened);
    reopened.keys("L");
    reopened.expect("Session tasks");
    reopened.expect("First live question");
    reopened.ready();
    reopened.keys("\x1b[B\r");
    reopened.expect("Task detail");
    reopened.expect("First live question");
    reopened.expect_absent("Second live question");
    reopened.ready();
    reopened.keys("\x1b");
    reopened.expect("Second live answer");
    reopened.ready();
    assert_order(&reopened);
    reopened.resize(22, 48);
    reopened.expect("Second live answer");
    reopened.capture("conversation-reopened-48");
    reopened.quit("\x03");
}

#[test]
#[ignore = "requires a matching app-server beside fliuno or at FLIUNO_TEST_APP_SERVER"]
fn markdown_code_is_readable_while_streaming_and_after_reopening() {
    use serde_json::json;
    use std::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let (complete, held) = channel();
    let provider = thread::spawn(move || {
        let mut socket = streaming_fixture_request(&listener);
        let answer = concat!(
            "## Read a project file\n\n",
            "Use `std::fs::read_to_string` to read the contents.\n\n",
            "- Keep the file path relative to your project.\n",
            "- Report an error if the file cannot be opened.\n\n",
            "```rust\n",
            "fn read_project_notes() -> std::io::Result<String> {\n",
            "    std::fs::read_to_string(\"\u{9879}\u{76ee}-notes-and-reference.txt\")\n",
            "}\n",
        );
        let delta = json!({"type":"response.output_text.delta","delta":answer});
        write!(socket, "data: {delta}\n\n").unwrap();
        socket.flush().unwrap();
        held.recv_timeout(Duration::from_secs(20)).unwrap();
        let final_answer = format!("{answer}```\n\nRead completed.");
        let event = json!({"type":"response.completed","response":{"status":"completed","output":[{"type":"message","content":[{"type":"output_text","text":final_answer}]}]}});
        write!(socket, "data: {event}\n\n").unwrap();
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
    terminal.keys("Show a Rust example for reading a file\r");
    terminal.expect("    std::fs::read_to_string(");
    terminal.expect("Keep the file path relative to your project.");
    terminal.expect_absent("```rust");
    terminal.capture("markdown-streaming-110");
    terminal.resize(25, 80);
    terminal.ready();
    terminal.expect("    std::fs::read_to_string(");
    terminal.capture("markdown-streaming-80");
    terminal.resize(25, 48);
    terminal.ready();
    terminal.expect("    std::fs::read_to_string(");
    terminal.capture("markdown-streaming-48");
    complete.send(()).unwrap();
    terminal.expect("Read completed.");
    terminal.quit("\x03");
    provider.join().unwrap();

    let mut reopened = Console::launch_with_config(server.as_deref(), root.path(), Some(&config));
    reopened.resize(25, 48);
    reopened.ready();
    reopened.expect("Read completed.");
    reopened.expect("    std::fs::read_to_string(");
    reopened.expect_absent("```rust");
    reopened.capture("markdown-reopened-48");
    reopened.quit("\x03");
}

#[test]
#[ignore = "requires a matching app-server beside fliuno or at FLIUNO_TEST_APP_SERVER"]
fn composer_commands_start_and_resume_conversations_without_consuming_literal_input() {
    let server = std::env::var_os("FLIUNO_TEST_APP_SERVER").map(PathBuf::from);
    let root = tempfile::tempdir().unwrap();
    let config = root.path().join("config.toml");
    std::fs::write(root.path().join("fixture.key"), "fixture-key\n").unwrap();
    std::fs::write(
        &config,
        r#"
        [workbench]
        database = "commands.db"
        [model]
        provider = "openai"
        name = "fixture"
        api_key_file = "fixture.key"
        base_url = "http://127.0.0.1:9/v1/"
    "#,
    )
    .unwrap();

    let mut console = Console::launch_with_config(server.as_deref(), root.path(), Some(&config));
    console.expect("What would you like to do in this folder?");
    console.resize(24, 80);
    console.ready();
    console.keys("/");
    console.expect("Start a new conversation");
    console.expect("/resume");
    console.expect("Message Fliuno");
    console.capture("composer-commands-80");
    console.keys("new\t");
    console.expect("Untitled session");
    console.expect("What would you like to do in this folder?");

    console.resize(22, 48);
    console.ready();
    console.keys("/resume\t");
    console.expect("Sessions");
    console.expect("Enter open");
    console.ready();
    console.capture("composer-resume-48");
    console.keys("\x1b[B\r");
    console.expect_absent("Sessions");
    console.expect("Fliuno  /  Conversation");
    console.expect("What would you like to do in this folder?");

    console.keys("/tmp/notes");
    console.expect("/tmp/notes");
    console.keys("\x1bOP");
    console.expect("Commands");
    console.keys("\x1b");
    console.expect_absent("Type to filter");
    console.expect("/tmp/notes");
    console.quit("\x03");

    let mut reopened = Console::launch_with_config(server.as_deref(), root.path(), Some(&config));
    reopened.expect("What would you like to do in this folder?");
    reopened.keys("\x1b[200~/new\x1b[201~");
    reopened.expect("/new");
    reopened.ready();
    if reopened.screen.screen().contents().contains("Tab run") {
        // ConPTY can deliver clipboard text as individual keys.
        reopened.keys("\r");
        reopened.expect_absent("Tab run");
    }
    reopened.expect("/new");
    reopened.quit("\x03");
}

#[test]
#[ignore = "requires a matching app-server beside fliuno or at FLIUNO_TEST_APP_SERVER"]
fn model_picker_switches_the_next_request_and_preserves_conversation_and_draft() {
    use serde_json::json;
    use std::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let (finish_first, completion) = channel();
    let (observed, requests) = channel();
    let provider = thread::spawn(move || {
        for answer in [
            "Original model answer",
            "Selected model answer",
            "Saved default answer",
        ] {
            let (mut socket, request) = streaming_fixture_input(&listener);
            observed.send(request).unwrap();
            if answer == "Original model answer" {
                completion.recv_timeout(Duration::from_secs(25)).unwrap();
            }
            let event = json!({"type":"response.completed","response":{"status":"completed","output":[{"type":"message","content":[{"type":"output_text","text":answer}]}]}});
            write!(socket, "data: {event}\n\n").unwrap();
            drop(socket);
            if answer == "Original model answer" {
                let listing = read_catalog_request(&listener, Some("fixture-key"));
                write_catalog_response(listing, 200, r#"{"data":[{"id":"second-model"}]}"#);
            }
        }
    });
    let server = std::env::var_os("FLIUNO_TEST_APP_SERVER").map(PathBuf::from);
    let root = tempfile::tempdir().unwrap();
    let config = root.path().join("config.toml");
    std::fs::write(root.path().join("fixture.key"), "fixture-key\n").unwrap();
    let settings = format!(
        r#"
        [workbench]
        database = "models.db"
        [model]
        provider = "openai"
        name = "first-model"
        api_key_file = "fixture.key"
        base_url = "http://{address}/v1/"
        [models.broken]
        provider = "openai"
        name = "missing-key-model"
        api_key_file = "missing.key"
        base_url = "http://{address}/v1/"
        [models.lab]
        provider = "openai"
        name = "second-model"
        reasoning_effort = "high"
        api_key_file = "fixture.key"
        base_url = "http://{address}/v1/"
        "#
    );
    std::fs::write(&config, &settings).unwrap();

    let mut console = Console::launch_with_config(server.as_deref(), root.path(), Some(&config));
    console.expect("Start a conversation");
    console.expect("first-model");
    console.keys("Retain the first prompt\r");
    let first = requests.recv_timeout(Duration::from_secs(12)).unwrap();
    assert_eq!(first["model"], "first-model");
    console.ready();
    console.keys("\x1bOPmodel");
    console.expect("Commands  /  model");
    console.ready();
    console.keys("\r");
    console.expect("second-model");
    console.ready();
    console.keys("\x1b[B\x1b[B\r");
    console.expect("Finish or resolve existing model work");
    console.ready();
    console.keys("\x1b");
    console.expect_absent("Switch model");
    finish_first.send(()).unwrap();
    console.expect("Original model answer");
    console.ready();

    console.keys("Send this preserved draft");
    console.keys("\x1bOPmodel");
    console.expect("Commands  /  model");
    console.ready();
    console.keys("\r");
    console.expect("Switch model");
    console.expect("second-model");
    console.ready();
    console.capture("model-picker-110");
    console.resize(24, 80);
    console.ready();
    console.capture("model-picker-80");
    console.resize(22, 48);
    console.ready();
    console.capture("model-picker-48");
    console.keys("\x1b[B\r");
    console.expect("Cannot read the configured");
    console.ready();
    console.capture("model-picker-error-48");
    console.keys("\x1b[15~");
    console.expect("Cannot read the configured API key.");
    console.keys("\x1b[B\x1b[15~");
    console.expect("This model is listed.");
    console.capture("model-connection-48");
    console.resize(24, 80);
    console.capture("model-connection-80");
    console.resize(28, 110);
    console.capture("model-connection-110");
    console.keys("\x1b");
    console.expect_absent("Switch model");
    console.expect("Send this preserved draft");
    console.keys("\x1bOPmodel");
    console.expect("Commands  /  model");
    console.ready();
    console.keys("\r");
    console.expect("second-model");
    console.ready();
    console.keys("\x1b[B\x1b[B\r");
    console.expect_absent("Switch model");
    console.expect("second-model");
    console.expect("Original model answer");
    console.expect("Send this preserved draft");
    console.ready();
    console.resize(22, 48);
    console.capture("model-switched-48");
    console.keys("\r");
    let second = requests.recv_timeout(Duration::from_secs(12)).unwrap();
    assert_eq!(second["model"], "second-model");
    assert_eq!(second["reasoning"]["effort"], "high");
    let input = second["input"].to_string();
    assert!(input.contains("Retain the first prompt"), "{input}");
    assert!(input.contains("Send this preserved draft"), "{input}");
    console.expect("Selected model answer");
    console.ready();
    console.quit("\x03");

    let mut reopened = Console::launch_with_config(server.as_deref(), root.path(), Some(&config));
    reopened.expect("first-model");
    reopened.expect("Selected model answer");
    assert_eq!(std::fs::read_to_string(&config).unwrap(), settings);
    reopened.expect("Press Enter to write a message");
    reopened.keys("n");
    reopened.expect("Enter Send");
    reopened.keys("Save without switching");
    reopened.expect("Save without switching");
    reopened.keys("\x1bOPmodel");
    reopened.expect("Commands  /  model");
    reopened.ready();
    reopened.keys("\r");
    reopened.expect("Ctrl+S Default");
    reopened.ready();
    reopened.resize(22, 48);
    reopened.keys("\x1b[B\x1b[B\x13");
    reopened.expect("Startup default saved.");
    reopened.expect("lab [default]");
    reopened.expect("[model] [now]");
    reopened.capture("model-default-saved-48");
    reopened.resize(24, 80);
    reopened.capture("model-default-saved-80");
    reopened.resize(28, 110);
    reopened.capture("model-default-saved-110");
    reopened.keys("\x1b");
    reopened.expect_absent("Switch model");
    reopened.expect("Save without switching");
    reopened.expect("Conversation  ·  first-model");
    reopened.quit("\x03");
    let updated = std::fs::read_to_string(&config).unwrap();
    assert_eq!(
        updated
            .lines()
            .filter(|line| !line.trim_start().starts_with("model_profile"))
            .collect::<Vec<_>>(),
        settings.lines().collect::<Vec<_>>()
    );
    let mut using_default =
        Console::launch_with_config(server.as_deref(), root.path(), Some(&config));
    using_default.expect("Conversation  ·  second-model");
    using_default.expect("Selected model answer");
    using_default.expect("Press Enter to write a message");
    using_default.keys("n");
    using_default.expect("Enter Send");
    // The composer is visible during the startup history refresh. Wait until
    // that command finishes before submitting, and verify the draft actually
    // reached the composer. A timeout should report the screen, including RPC errors.
    using_default.keys("Use the saved default.");
    using_default.expect("Use the saved default.");
    using_default.ready();
    using_default.keys("\r");
    using_default.expect("Saved default answer");
    let third = requests.recv_timeout(Duration::from_secs(1)).unwrap();
    assert_eq!(third["model"], "second-model");
    assert_eq!(third["reasoning"]["effort"], "high");
    assert!(
        third["input"]
            .to_string()
            .contains("Use the saved default.")
    );
    using_default.quit("\x03");
    provider.join().unwrap();
}

#[test]
#[ignore = "requires a matching app-server beside fliuno or at FLIUNO_TEST_APP_SERVER"]
fn model_editor_browses_an_existing_route_and_applies_saved_settings_after_reopening() {
    use serde_json::json;
    use std::net::TcpListener;

    let server = std::env::var_os("FLIUNO_TEST_APP_SERVER").map(PathBuf::from);
    let root = tempfile::tempdir().unwrap();
    let config = root.path().join("config.toml");
    std::fs::write(root.path().join("fixture.key"), "fixture-editor-key\n").unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}/v1/", listener.local_addr().unwrap());
    let settings = format!(
        r#"[model]
provider = "openai"
name = "startup-model" # keep this note
base_url = "{base}"
api_key_file = "fixture.key"
"#
    );
    std::fs::write(&config, &settings).unwrap();
    let (release, wait_for_search) = channel();
    let (observed, generation) = channel();
    let provider = thread::spawn(move || {
        let listing = read_catalog_request(&listener, Some("fixture-editor-key"));
        wait_for_search
            .recv_timeout(Duration::from_secs(4))
            .unwrap();
        write_catalog_response(
            listing,
            200,
            r#"{"data":[{"id":"other-model"},{"id":"found-model"}]}"#,
        );
        let (mut socket, request) = streaming_fixture_input(&listener);
        observed.send(request).unwrap();
        let event = json!({"type":"response.completed","response":{"status":"completed","output":[{"type":"message","content":[{"type":"output_text","text":"Edited model replied"}]}]}});
        write!(socket, "data: {event}\n\n").unwrap();
    });
    let mut console = Console::launch_with_config(server.as_deref(), root.path(), Some(&config));
    console.expect("Start a conversation");
    console.keys("Draft survives edits");
    console.expect("Draft survives edits");
    let open_picker = |console: &mut Console| {
        console.keys("\x1bOPmodel");
        console.expect("Commands  /  model");
        console.keys("\r");
        console.expect("F2 Edit profile");
    };
    open_picker(&mut console);
    console.keys("\x1bOQ");
    console.expect("Edit model profile");
    console.keys("\x1b[15~");
    console.expect("Loading the model catalog");
    console.keys("found");
    console.expect("› found");
    release.send(()).unwrap();
    console.expect("found-model");
    console.expect_absent("other-model");
    console.resize(22, 48);
    console.capture("workbench-model-catalog-48");
    console.resize(24, 80);
    console.capture("workbench-model-catalog-80");
    console.resize(28, 110);
    console.capture("workbench-model-catalog-110");
    console.keys("\r");
    console.expect("Edit model profile");
    console.expect("found-model");
    console.keys("\x1bOR");
    console.expect("Set reasoning effort");
    console.keys(&format!("{}\r", "\x1b[B".repeat(5)));
    console.expect("Reasoning: high");
    console.resize(22, 48);
    console.capture("workbench-model-edit-48");
    console.resize(24, 80);
    console.capture("workbench-model-edit-80");
    console.resize(28, 110);
    console.capture("workbench-model-edit-110");
    console.keys("\x13");
    console.expect("Profile saved. Enter to use it now.");
    console.expect("Conversation  ·  startup-model");
    console.keys("\x1b");
    console.expect_absent("Switch model");
    console.expect("Draft survives edits");
    let saved = std::fs::read_to_string(&config).unwrap();
    assert!(saved.contains("name = \"found-model\" # keep this note"));
    assert!(saved.contains("reasoning_effort = \"high\""));
    assert!(saved.contains(&format!("base_url = \"{base}\"")));
    assert!(saved.contains("api_key_file = \"fixture.key\""));
    open_picker(&mut console);
    console.expect("[model] [default] [saved]");
    console.keys("\r");
    console.expect("Conversation  ·  found-model");
    console.expect("Draft survives edits");
    console.keys("\r");
    let actual = generation.recv_timeout(Duration::from_secs(12)).unwrap();
    assert_eq!(actual["model"], "found-model");
    assert_eq!(actual["reasoning"]["effort"], "high");
    assert!(actual["input"].to_string().contains("Draft survives edits"));
    console.expect("Edited model replied");
    console.quit("\x03");
    provider.join().unwrap();
}

#[test]
#[ignore = "requires a matching app-server beside fliuno or at FLIUNO_TEST_APP_SERVER"]
fn workbench_adds_a_connection_and_rotates_its_route_without_reusing_credentials() {
    for viewport in [(22, 48), (24, 80), (28, 110)] {
        manage_connection_at_width(viewport);
    }
}

fn manage_connection_at_width(viewport: (u16, u16)) {
    let (_, cols) = viewport;
    use std::net::TcpListener;
    let server = std::env::var_os("FLIUNO_TEST_APP_SERVER").map(PathBuf::from);
    let root = tempfile::tempdir().unwrap();
    let config = root.path().join("config.toml");
    let baseline =
        "[model]\nprovider = 'openai'\nname = 'original-model'\napi_key_file = 'original.key'\n";
    std::fs::write(&config, baseline).unwrap();
    std::fs::write(root.path().join("original.key"), "original-fixture-key").unwrap();

    let local = TcpListener::bind("127.0.0.1:0").unwrap();
    let local_url = format!("http://{}/v1/", local.local_addr().unwrap());
    let rotated = TcpListener::bind("127.0.0.1:0").unwrap();
    let next_url = format!("http://{}/v1/", rotated.local_addr().unwrap());
    let (dispatched, generation) = channel();
    let provider = thread::spawn(move || {
        write_catalog_response(
            read_catalog_request(&local, None),
            200,
            r#"{"data":[{"id":"local-model"}]}"#,
        );
        write_catalog_response(
            read_catalog_request(&rotated, Some("replacement-fixture-key")),
            200,
            r#"{"data":[{"id":"local-model","name":"replacement-fixture-key"}]}"#,
        );
        let (mut socket, request) = streaming_fixture_input(&rotated);
        dispatched.send(request).unwrap();
        socket.write_all(concat!(
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Updated connection replied.\"},\"finish_reason\":null}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
            "data: [DONE]\n\n",
        ).as_bytes()).unwrap();
    });
    let mut console =
        Console::launch_with_config_at(server.as_deref(), root.path(), Some(&config), viewport);
    console.expect("Enter Send");
    let open = |c: &mut Console| {
        c.keys("\x1bOPmodel");
        c.expect("Commands  /  model");
        c.keys("\r");
        c.expect("Ctrl+N Add profile");
    };
    console.keys("My draft survives connection changes.");
    open(&mut console);
    console.keys("\x0e");
    console.expect("Add model profile");
    console.keys("local\r");
    console.expect("API protocol");
    console.keys("\x1b[B\r");
    console.expect("Base URL");
    console.keys(&format!("{local_url}\r"));
    console.expect("Connect without a key");
    console.expect_absent("Keep current settings");
    console.capture(&format!("connection-auth-{cols}"));
    console.keys("\x1b[B\x1b[B\r");
    console.expect("Model and save");
    console.keys("\x1b[15~");
    console.expect("Browse connection catalog");
    console.expect("local-model");
    console.capture(&format!("connection-catalog-{cols}"));
    assert_eq!(std::fs::read_to_string(&config).unwrap(), baseline);
    console.keys("\r");
    console.expect("Model and save");
    console.keys("\r");
    console.expect("Profile saved. Enter to use it now.");
    console.expect("[model] [now/default]");
    let settings = fliuno_config::Settings::load(&config).unwrap();
    assert!(
        settings
            .named_model("local")
            .unwrap()
            .read_model_key()
            .unwrap()
            .is_none()
    );
    assert!(settings.workbench.model_profile.is_none());
    console.keys("\r");
    console.expect_absent("Switch model");
    console.expect("Using local-model");
    console.ready();

    open(&mut console);
    console.keys("\x1bOS");
    console.expect("API protocol");
    console.keys("\r");
    console.expect("Base URL");
    console.keys(&"\x7f".repeat(local_url.len()));
    console.keys(&format!("{next_url}\r"));
    console.expect("Choose credentials for this connection.");
    console.expect_absent("Keep current settings");
    console.keys("\r");
    console.expect("Paste API key");
    console.keys("replacement-fixture-key");
    console.expect("••••••••");
    console.expect_absent("replacement-fixture-key");
    console.capture(&format!("connection-masked-key-{cols}"));
    console.keys("\r");
    console.expect("Model and save");
    console.keys("\x1b[15~");
    console.expect("Browse connection catalog");
    console.expect("> local-model");
    console.expect_absent("replacement-fixture-key");
    console.keys("\r");
    console.expect("Model and save");

    // Save with an existing key file: the preview above exercised pasted-key protection.
    console.keys("\x1b");
    console.expect("Connection · API key");
    console.expect("••••••••");
    console.keys("\x1b");
    console.expect("Authentication");
    std::fs::write(
        root.path().join("replacement.key"),
        "replacement-fixture-key",
    )
    .unwrap();
    console.keys("\x1b[B\r");
    console.expect("Path to key file");
    console.keys("replacement.key\r");
    console.expect("Model and save");
    console.keys("\r");
    console.expect("Profile saved. Enter to use it now.");
    let saved = std::fs::read_to_string(&config).unwrap();
    assert!(saved.starts_with(baseline));
    assert!(!saved.contains("replacement-fixture-key"));
    let settings = fliuno_config::Settings::load(&config).unwrap();
    let model = settings.named_model("local").unwrap();
    assert_eq!(model.base_url.as_deref(), Some(next_url.as_str()));
    assert_eq!(model.read_api_key().unwrap(), "replacement-fixture-key");

    // Reopen the picker before selecting to verify reload even with unchanged model ID.
    console.keys("\x1b");
    open(&mut console);
    console.keys("\r");
    console.expect_absent("Switch model");
    console.expect("Using local-model");
    console.expect("Enter Send");
    console.ready();
    console.keys("\r");
    console.expect("Updated connection replied.");
    let request = generation.recv_timeout(Duration::from_secs(15)).unwrap();
    assert_eq!(request["model"], "local-model");
    assert!(
        request["messages"]
            .to_string()
            .contains("My draft survives connection changes.")
    );
    console.quit("\x03");
    provider.join().unwrap();
}

#[test]
#[ignore = "requires a matching app-server beside fliuno or at FLIUNO_TEST_APP_SERVER"]
fn reopening_waits_for_history_and_honors_one_cancellable_send() {
    use fliuno_client::Client;
    use serde_json::{Value, json};
    use std::{fs, io::BufReader, process::Stdio};

    let root = tempfile::tempdir().unwrap();
    let server = std::env::var_os("FLIUNO_TEST_APP_SERVER")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            binary().with_file_name(format!("fliuno-app-server{}", std::env::consts::EXE_SUFFIX))
        });
    let relay = root
        .path()
        .join(format!("relay{}", std::env::consts::EXE_SUFFIX));
    let compiler = std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into());
    let compile = Command::new(compiler)
        .args(["--edition=2024", "-o"])
        .arg(&relay)
        .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/slow_history.rs"))
        .output()
        .unwrap();
    assert!(
        compile.status.success(),
        "{}",
        String::from_utf8_lossy(&compile.stderr)
    );

    let config = root.path().join("config.toml");
    let database = root.path().join("history.db");
    fs::write(root.path().join("fixture.key"), "test-only\n").unwrap();
    fs::write(
        &config,
        r#"
[workbench]
database = "history.db"
[model]
provider = "openai"
name = "fixture-model"
base_url = "http://127.0.0.1:9/v1/"
api_key_file = "fixture.key"
"#,
    )
    .unwrap();
    let mut seed = Command::new(&server)
        .arg("--db")
        .arg(&database)
        .arg("--demo")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut client = Client::connect(
        BufReader::new(seed.stdout.take().unwrap()),
        seed.stdin.take().unwrap(),
        "reopen-fixture",
        |_| {},
    )
    .unwrap();
    let workspace: Value = client
        .call("workspace/open", &json!({"root":root.path()}), |_| {})
        .unwrap();
    let mut sessions = Vec::new();
    for title in ["Other conversation", "Saved conversation"] {
        let session: Value = client
            .call(
                "session/create",
                &json!({"workspaceId":workspace["id"],"title":title}),
                |_| {},
            )
            .unwrap();
        sessions.push(session);
    }
    let current = &sessions[1]["id"];
    let task: Value = client
        .call(
            "task/start",
            &json!({"sessionId":current,"input":"Previous turn","context":{"planOnly":true}}),
            |_| {},
        )
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let task: Value = client
            .call("task/get", &json!({"taskId":task["id"]}), |_| {})
            .unwrap();
        if task["status"] == "completed" {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "previous turn did not complete: {task}"
        );
        thread::sleep(Duration::from_millis(25));
    }
    drop(client);
    assert!(seed.wait().unwrap().success());

    for (action, viewport, delay) in [
        ("exit", (28, 110), "session/listHistory"),
        ("disconnect", (24, 80), "session/listHistory"),
        ("cancel", (24, 80), "task/get"),
        ("switch", (28, 110), "session/listHistory"),
        ("send", (22, 48), "session/listHistory"),
    ] {
        let log = root.path().join(action);
        fs::create_dir(&log).unwrap();
        let mut command = CommandBuilder::new(binary());
        command.arg("--config");
        command.arg(&config);
        command.env("FLIUNO_RELAY_DIR", &log);
        command.env("FLIUNO_RELAY_SERVER", &server);
        command.env("FLIUNO_RELAY_DELAY", delay);
        let mut console = Console::spawn_at(command, Some(&relay), root.path(), viewport);
        console.expect("Opening conversation");
        let deadline = Instant::now() + Duration::from_secs(10);
        while !log.join("paused").exists() {
            assert!(Instant::now() < deadline, "{delay} was not paused");
            thread::sleep(Duration::from_millis(10));
        }
        console.keys("\rFollow-up while loading\r\r");
        console.expect("Will send when conversation opens");
        console.expect("Esc Cancel send");
        console.capture(&format!("reopening-{action}-{}", viewport.1));
        assert!(!log.join("starts.jsonl").exists());

        match action {
            "exit" => {
                console.quit("\x03");
                assert!(!log.join("starts.jsonl").exists());
                continue;
            }
            "cancel" => {
                console.keys("\x1b");
                console.expect_absent("Will send when conversation opens");
            }
            "switch" => {
                console.keys("\x1bOP");
                console.expect("Type to filter");
            }
            _ => {}
        }
        if action == "disconnect" {
            fs::write(log.join("disconnect"), []).unwrap();
        }
        fs::write(log.join("continue"), []).unwrap();
        match action {
            "disconnect" => {
                console.expect("Connection lost");
                console.expect("Follow-up while loading");
            }
            "send" => {
                console.expect("Previous turn");
                console.expect_absent("Will send when conversation opens");
                console.ready();
            }
            "cancel" => {
                console.expect("Send cancelled. Draft kept.");
                console.expect("Follow-up while loading");
            }
            "switch" => {
                console.expect("Previous turn");
                console.ready();
                console.keys("resume\r");
                console.expect("Other conversation");
                console.ready();
                console.keys("\x1b[B\r");
                console.expect_absent("Sessions");
                console.expect_absent("Follow-up while loading");
                console.ready();
            }
            _ => unreachable!(),
        }
        console.quit("\x03");
        if action == "send" {
            let sent = fs::read_to_string(log.join("starts.jsonl")).unwrap();
            let requests = sent
                .lines()
                .map(|line| serde_json::from_str::<Value>(line).unwrap())
                .collect::<Vec<_>>();
            assert_eq!(requests.len(), 1, "{sent}");
            let params = &requests[0]["params"];
            assert_eq!(params["sessionId"], *current);
            assert_eq!(params["input"], "Follow-up while loading");
            assert!(uuid::Uuid::parse_str(params["clientRequestId"].as_str().unwrap()).is_ok());
        } else {
            assert!(
                !log.join("starts.jsonl").exists(),
                "unexpected send after {action}"
            );
        }
    }
}
