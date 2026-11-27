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
        let pair = native_pty_system().openpty(size(28, 110)).unwrap();
        let mut command = CommandBuilder::new(binary());
        if let Some(config) = config {
            command.arg("--config");
            command.arg(config);
        } else {
            command.arg("--demo");
            command.arg("--db");
            command.arg(root.join("workbench.db"));
        }
        if let Some(server) = server {
            command.arg("--server");
            command.arg(server);
        }
        command.arg("--workspace");
        command.arg(root);
        command.env("TERM", "xterm-256color");
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
    let mut process = Command::new(server)
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
    console.capture("backup-restored-narrow");
    console.keys("r");
    console.expect("Restore from backup folder");
    console.keys(&format!("{}\r", backup.display()));
    console.expect("Restore to a new database path");
    console.keys(&format!("{}\r", restored.display()));
    console.expect("restore destination already");
    console.expect("restore to a new database path");
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
