use super::*;
use std::{
    io::{Cursor, Error, ErrorKind},
    sync::atomic::{AtomicUsize, Ordering},
};

struct Growing<'a>(&'a AtomicUsize);
impl Read for Growing<'_> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        buffer.fill(b'x');
        self.0.fetch_add(buffer.len(), Ordering::Relaxed);
        Ok(buffer.len())
    }
}

#[test]
fn byte_budget_accepts_exact_limit_and_stops_an_unbounded_reader_after_one_extra_byte() {
    let token = CancellationToken::new();
    let bytes = vec![b'x'; READ_LIMIT as usize];
    assert_eq!(read_bounded(Cursor::new(&bytes), &token).unwrap(), bytes);
    assert!(read_bounded(Cursor::new([]), &token).unwrap().is_empty());
    let consumed = AtomicUsize::new(0);
    assert!(matches!(
        read_bounded(Growing(&consumed), &token),
        Err(ToolError::Failed(_))
    ));
    assert_eq!(consumed.load(Ordering::Relaxed), READ_LIMIT as usize + 1);
}

struct CancelOnRead<'a> {
    token: &'a CancellationToken,
    eof: bool,
}
impl Read for CancelOnRead<'_> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        self.token.cancel();
        buffer[0] = b'x';
        Ok(usize::from(!self.eof))
    }
}

#[test]
fn cancellation_before_read_after_a_chunk_and_at_eof_never_returns_partial_success() {
    let token = CancellationToken::new();
    token.cancel();
    let consumed = AtomicUsize::new(0);
    assert!(read_bounded(Growing(&consumed), &token).is_err());
    assert_eq!(consumed.load(Ordering::Relaxed), 0);
    for eof in [false, true] {
        let token = CancellationToken::new();
        assert!(matches!(
            read_bounded(CancelOnRead { token: &token, eof }, &token),
            Err(ToolError::Failed(_))
        ));
    }
}

struct InterruptedOnce(Option<Cursor<Vec<u8>>>);
impl Read for InterruptedOnce {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        if let Some(reader) = &mut self.0 {
            reader.read(buffer)
        } else {
            self.0 = Some(Cursor::new(b"complete".to_vec()));
            Err(Error::from(ErrorKind::Interrupted))
        }
    }
}

#[test]
fn interrupted_reads_retry_but_other_errors_discard_the_snapshot() {
    let token = CancellationToken::new();
    assert_eq!(
        read_bounded(InterruptedOnce(None), &token).unwrap(),
        b"complete"
    );
    struct Failed(bool);
    impl Read for Failed {
        fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
            if self.0 {
                Err(Error::from(ErrorKind::PermissionDenied))
            } else {
                self.0 = true;
                buffer[0] = b'x';
                Ok(1)
            }
        }
    }
    assert!(matches!(
        read_bounded(Failed(false), &token),
        Err(ToolError::Failed(_))
    ));
}

struct Directory(std::path::PathBuf);
impl Directory {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "fliuno-file-read-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&root).unwrap();
        Self(root)
    }
}
impl Drop for Directory {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).unwrap();
    }
}

#[test]
fn actual_file_limit_applies_before_line_selection_and_write_review() {
    use crate::{Operation, Tool, ToolCall, ToolContext, WorkspaceTool, json, sha256};
    use fliuno_protocol::Workspace;
    use std::sync::{Arc, Mutex};
    let directory = Directory::new();
    let target = directory.0.join("report.txt");
    let bytes = vec![b'x'; READ_LIMIT as usize];
    std::fs::write(&target, &bytes).unwrap();
    let workspace = Workspace {
        id: "workspace".into(),
        root: directory.0.to_str().unwrap().into(),
    };
    let context = ToolContext {
        task_id: "task",
        session_id: "session",
        workspace: &workspace,
    };
    let token = CancellationToken::new();
    let tool = WorkspaceTool::new(Operation::Read, Arc::new(Mutex::new(())));
    let call = ToolCall {
        id: "read".into(),
        name: "workspace_read_text".into(),
        arguments: json!({"path":"report.txt","line":99,"limit":1}),
    };
    let output = tool.execute(&context, &call, &token).unwrap().output;
    assert_eq!(output["content"], "");
    assert_eq!(output["sha256"], sha256(&bytes));
    assert!(
        crate::prepare_workspace_write(&directory.0, "review", "report.txt", "new", &token).is_ok()
    );
    std::fs::write(&target, vec![b'x'; READ_LIMIT as usize + 1]).unwrap();
    assert!(tool.execute(&context, &call, &token).is_err());
    assert!(
        crate::prepare_workspace_write(&directory.0, "review", "report.txt", "new", &token)
            .is_err()
    );
    assert_eq!(std::fs::metadata(&target).unwrap().len(), READ_LIMIT + 1);
}

// Execute this probe in its own test process so a regression in open(2) cannot
// hang the entire runner. No writer ever opens the actual FIFO.
#[cfg(unix)]
#[test]
fn fifo_without_a_writer_is_rejected_by_read_patch_and_write_review_without_hanging() {
    use crate::{Operation, Tool, ToolCall, ToolContext, WorkspaceTool, json};
    use cap_std::{ambient_authority, fs::Dir};
    use fliuno_protocol::Workspace;
    use std::{
        process::{Command, Stdio},
        sync::{Arc, Mutex},
        time::{Duration, Instant},
    };
    const PROBE: &str = "FLIUNO_FILE_FIFO_PROBE";
    if let Some(root) = std::env::var_os(PROBE) {
        let directory = Dir::open_ambient_dir(&root, ambient_authority()).unwrap();
        for writable in [false, true] {
            let mut file = directory.open_with("fifo", &options(writable)).unwrap();
            assert!(read(&mut file, &CancellationToken::new()).is_err());
        }
        let workspace = Workspace {
            id: "workspace".into(),
            root: root.to_str().unwrap().into(),
        };
        let context = ToolContext {
            task_id: "task",
            session_id: "session",
            workspace: &workspace,
        };
        let token = CancellationToken::new();
        for operation in [
            Operation::Read,
            Operation::ReadBinary,
            Operation::Patch,
            Operation::Replace,
        ] {
            let tool = WorkspaceTool::new(operation, Arc::new(Mutex::new(())));
            let arguments = match operation {
                Operation::Read => json!({"path":"fifo"}),
                Operation::ReadBinary => json!({"path":"fifo"}),
                Operation::Patch => json!({"path":"fifo","baseSha256":"0".repeat(64),
                    "patch":"--- a/fifo\n+++ b/fifo\n@@ -1 +1 @@\n-old\n+new\n"}),
                Operation::Replace => {
                    json!({"path":"fifo","content":"new","baseSha256":"0".repeat(64)})
                }
                _ => unreachable!(),
            };
            let call = ToolCall {
                id: "probe".into(),
                name: tool.spec().name,
                arguments,
            };
            assert!(matches!(
                tool.execute(&context, &call, &token),
                Err(ToolError::Failed(_))
            ));
        }
        assert!(
            crate::prepare_workspace_write(
                std::path::Path::new(&root),
                "review",
                "fifo",
                "new",
                &token
            )
            .is_err()
        );
        return;
    }
    let directory = Directory::new();
    assert!(
        Command::new("mkfifo")
            .arg(directory.0.join("fifo"))
            .status()
            .unwrap()
            .success()
    );
    let mut child = Command::new(std::env::current_exe().unwrap())
        .arg("--exact")
        .arg("file_io::tests::fifo_without_a_writer_is_rejected_by_read_patch_and_write_review_without_hanging")
        .arg("--nocapture")
        .env(PROBE, &directory.0)
        .stdin(Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(status.success(), "FIFO probe failed: {status}");
            break;
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("workspace FIFO probe blocked without a writer");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}
