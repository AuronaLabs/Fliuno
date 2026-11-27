//! Local backup utilities use the sibling app-server's database commands.
use crate::options::Options;
use serde::Deserialize;
use std::{
    fs,
    io::{Read, Seek},
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::{Arc, Mutex},
    thread,
    time::Duration,
};

#[derive(Debug, Clone)]
pub enum BackupRequest {
    Create(PathBuf),
    Verify(PathBuf),
    Restore {
        source: PathBuf,
        destination: PathBuf,
    },
}

#[derive(Debug)]
pub struct BackupResult {
    pub request: BackupRequest,
    pub manifest: BackupManifest,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BackupManifest {
    pub schema_version: u32,
    pub database_bytes: u64,
    pub database_sha256: String,
    pub event_through_sequence: u64,
    pub task_count: u64,
    pub artifact_count: u64,
}

#[derive(Deserialize)]
struct Report {
    manifest: BackupManifest,
}

#[derive(Default)]
pub struct BackupProcess {
    child: Option<Child>,
    closed: bool,
}

impl BackupProcess {
    pub fn close(&mut self) {
        self.closed = true;
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

pub type ProcessSlot = Arc<Mutex<BackupProcess>>;

impl BackupRequest {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Create(_) => "Backup created",
            Self::Verify(_) => "Backup verified",
            Self::Restore { .. } => "Database restored",
        }
    }

    pub fn result_path(&self) -> &std::path::Path {
        match self {
            Self::Create(path) | Self::Verify(path) => path,
            Self::Restore { destination, .. } => destination,
        }
    }
}

pub fn run(
    options: &Options,
    request: BackupRequest,
    slot: &ProcessSlot,
) -> Result<BackupResult, Box<dyn std::error::Error>> {
    let mut output = tempfile::tempfile()?;
    let mut errors = tempfile::tempfile()?;
    let mut command = Command::new(fs::canonicalize(&options.server)?);
    command
        .arg("backup")
        .stdin(Stdio::null())
        .stdout(output.try_clone()?)
        .stderr(errors.try_clone()?);
    match &request {
        BackupRequest::Create(destination) => {
            command
                .arg("create")
                .arg(&options.database)
                .arg(destination)
                .args(["--timeout-ms", "60000"]);
        }
        BackupRequest::Verify(source) => {
            command.arg("verify").arg(source);
        }
        BackupRequest::Restore {
            source,
            destination,
        } => {
            command.arg("restore").arg(source).arg(destination);
        }
    }
    let status = {
        let mut process = slot.lock().map_err(|_| "Backup process lock unavailable")?;
        if process.closed {
            return Err("Workbench is closing".into());
        }
        process.child = Some(command.spawn()?);
        drop(process);
        loop {
            let mut process = slot.lock().map_err(|_| "Backup process lock unavailable")?;
            let child = process
                .child
                .as_mut()
                .ok_or("Backup command was interrupted")?;
            if let Some(status) = child.try_wait()? {
                process.child.take();
                break status;
            }
            drop(process);
            thread::sleep(Duration::from_millis(40));
        }
    };
    if !status.success() {
        errors.rewind()?;
        let mut message = Vec::new();
        errors.take(4096).read_to_end(&mut message)?;
        let message = String::from_utf8_lossy(&message);
        return Err(format!("Backup command {status}: {}", message.trim()).into());
    }
    output.rewind()?;
    let report: Report = serde_json::from_reader(output.take(16 * 1024))?;
    Ok(BackupResult {
        request,
        manifest: report.manifest,
    })
}
