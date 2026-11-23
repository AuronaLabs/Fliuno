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
        let pair = native_pty_system().openpty(size(28, 110)).unwrap();
        let mut command = CommandBuilder::new(binary());
        command.arg("--demo");
        if let Some(server) = server {
            command.arg("--server");
            command.arg(server);
        }
        command.arg("--workspace");
        command.arg(root);
        command.arg("--db");
        command.arg(root.join("workbench.db"));
        command.env("TERM", "xterm-256color");
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
        let deadline = Instant::now() + Duration::from_secs(20);
        let mut transcript = Vec::new();
        while !self.screen.screen().contents().contains(expected) {
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
                    "expected {expected:?}: {error}\nchild: {:?}\nscreen: {}\noutput: {:?}",
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
            std::fs::write(
                PathBuf::from(directory).join(format!("{name}.ansi")),
                self.screen.screen().contents_formatted(),
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
    console.expect("Disconnected");
    let until = Instant::now() + Duration::from_secs(10);
    while !root.path().join("workbench.db.started").exists() {
        assert!(Instant::now() < until, "no initialize request");
        thread::sleep(Duration::from_millis(20));
    }
    console.quit("q");
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
    console.keys("/");
    console.expect("Commands  /");
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
    console.expect("Allow once / Deny");
    console.expect("artifact_create");
    console.keys("y");
    console.expect("Completed");
    console.ready();
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
    console.expect("Task queue");
    console.keys("\t");
    console.expect("Task detail");
    console.ready();
    assert!(!console.screen.screen().contents().contains("Task queue"));
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
    reopened.expect("Task queue");
    reopened.keys("a");
    reopened.expect("Allow once / Deny");
    reopened.expect("artifact_create");
    reopened.capture("narrow-approval");
    reopened.keys("n");
    reopened.expect("Failed");
    reopened.keys("\t");
    reopened.expect("Failure reason");
    reopened.expect("tool execution denied");
    reopened.ready();
    reopened.resize(28, 110);
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
