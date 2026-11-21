use fliuno_core::{DemoProvider, Kernel};
use fliuno_protocol::{ApprovalDecision, TaskStatus};
use serde_json::Value;
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
};

struct Fixture {
    root: PathBuf,
    backup: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("fliuno-backup-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&root).unwrap();
        let fixture = Self {
            backup: root.join("backup #with% spaces"),
            root,
        };
        let mut kernel =
            Kernel::open(fixture.root.join("source.db"), Box::new(DemoProvider)).unwrap();
        let workspace = kernel
            .open_workspace(fixture.root.to_str().unwrap())
            .unwrap();
        let session = kernel.create_session(&workspace.id).unwrap();
        let task = kernel.start_task(&session.id, "restored artifact").unwrap();
        kernel
            .respond_approval(
                &task.id,
                &task.pending_approval.unwrap().id,
                ApprovalDecision::AllowOnce,
            )
            .unwrap();
        let pending = kernel.start_task(&session.id, "await approval").unwrap();
        assert_eq!(pending.status, TaskStatus::WaitingApproval);
        let (running, _) = kernel
            .create_task_with_key(&session.id, "not dispatched", None)
            .unwrap();
        assert_eq!(running.status, TaskStatus::Running);
        kernel.create_backup(&fixture.backup).unwrap();
        fixture
    }

    fn run(&self, operation: &str, destination: Option<&Path>) -> Output {
        self.command()
            .arg(operation)
            .arg(&self.backup)
            .args(destination)
            .output()
            .unwrap()
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_fliuno-app-server"));
        command
            .arg("backup")
            .current_dir(&self.root)
            .env(
                "FLIUNO_PROVIDER",
                "invalid-provider-maintenance-must-not-use",
            )
            .stdin(Stdio::null());
        command
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn report(output: Output, operation: &str) -> Value {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stderr.is_empty());
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["operation"], operation);
    report["manifest"].clone()
}

fn reject(output: Output, expected: &str) {
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(error.contains(expected), "{error}");
}

#[test]
fn maintenance_verifies_and_restores_without_starting_the_kernel() {
    let fixture = Fixture::new();
    let destination = fixture.root.join("restored database.db");
    let snapshot = fs::read(fixture.backup.join("database.sqlite")).unwrap();
    // A snapshot made by v0.0.4 on Windows can contain an empty WAL and SHM.
    fs::write(fixture.backup.join("database.sqlite-wal"), []).unwrap();
    fs::write(fixture.backup.join("database.sqlite-shm"), [0; 32 * 1024]).unwrap();
    let manifest = report(fixture.run("verify", None), "verify");
    assert_eq!(manifest["taskCount"], 3);
    assert_eq!(manifest["artifactCount"], 1);
    assert_eq!(manifest["artifactBytes"], "restored artifact".len());
    assert_eq!(
        report(fixture.run("restore", Some(&destination)), "restore"),
        manifest
    );
    assert_eq!(
        report(
            fixture.run("verify-restored", Some(&destination)),
            "verify-restored"
        ),
        manifest
    );
    assert_eq!(fs::read(&destination).unwrap(), snapshot);
    assert!(!fixture.root.join("restored database.db-wal").exists());
    assert!(!fixture.root.join("restored database.db-shm").exists());
    assert_eq!(
        fs::read(fixture.backup.join("database.sqlite")).unwrap(),
        snapshot
    );
    assert!(!fixture.root.join(".fliuno").exists());

    let activated = Command::new(env!("CARGO_BIN_EXE_fliuno-app-server"))
        .arg("--db")
        .arg(&destination)
        .current_dir(&fixture.root)
        .env("FLIUNO_PROVIDER", "demo")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(
        activated.status.success(),
        "{}",
        String::from_utf8_lossy(&activated.stderr)
    );
    let db = rusqlite::Connection::open_with_flags(
        &destination,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    let mut stmt = db
        .prepare("SELECT json_extract(data, '$.status') FROM tasks ORDER BY 1")
        .unwrap();
    let statuses = stmt
        .query_map([], |row| row.get::<_, String>(0))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(statuses, ["completed", "interrupted", "waiting_approval"]);
    let count: i64 = db
        .query_row("SELECT COUNT(*) FROM artifact_bodies", [], |row| row.get(0))
        .unwrap();
    assert_eq!(count, 1);
}

#[test]
fn maintenance_rejects_corruption_and_preserves_existing_files() {
    let fixture = Fixture::new();
    let destination = fixture.root.join("existing.db");
    fs::write(&destination, b"unrelated data").unwrap();
    reject(
        fixture.run("restore", Some(&destination)),
        "destination already exists",
    );
    assert_eq!(fs::read(&destination).unwrap(), b"unrelated data");

    let destination = fixture.root.join("new.db");
    let sidecar = destination.with_file_name("new.db-wal");
    fs::write(&sidecar, b"orphaned journal").unwrap();
    reject(
        fixture.run("restore", Some(&destination)),
        "SQLite sidecar files",
    );
    assert!(!destination.exists());
    assert_eq!(fs::read(&sidecar).unwrap(), b"orphaned journal");
    fs::remove_file(sidecar).unwrap();

    let wal = fixture.backup.join("database.sqlite-wal");
    fs::write(&wal, b"not an empty WAL").unwrap();
    reject(fixture.run("verify", None), "nonempty SQLite WAL");
    reject(
        fixture.run("restore", Some(&destination)),
        "nonempty SQLite WAL",
    );
    assert!(!destination.exists());
    fs::remove_file(wal).unwrap();

    let database = fixture.backup.join("database.sqlite");
    let mut bytes = fs::read(&database).unwrap();
    let last = bytes.last_mut().unwrap();
    *last ^= 1;
    fs::write(&database, bytes).unwrap();
    reject(
        fixture.run("verify", None),
        "does not match its backup manifest",
    );
    reject(
        fixture.run("restore", Some(&destination)),
        "does not match its backup manifest",
    );
    assert!(!destination.exists());
    assert!(!fs::read_dir(&fixture.root).unwrap().any(|entry| {
        entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".fliuno-restore-")
    }));
}

#[test]
fn maintenance_creates_online_cancels_before_publication_and_rotates_managed_backups() {
    let fixture = Fixture::new();
    let source = fixture.root.join("online.db");
    let mut kernel = Kernel::open(&source, Box::new(DemoProvider)).unwrap();
    let workspace = kernel
        .open_workspace(fixture.root.to_str().unwrap())
        .unwrap();
    let session = kernel.create_session(&workspace.id).unwrap();
    let (undispatched, _) = kernel
        .create_task_with_key(&session.id, "leave running", None)
        .unwrap();
    let target = fixture.root.join("manual");
    let create = |flags: &[&str]| {
        fixture
            .command()
            .arg("create")
            .arg(&source)
            .arg(&target)
            .args(flags)
            .output()
            .unwrap()
    };
    reject(
        create(&["--timeout-ms", "0"]),
        "timed out before publication",
    );
    assert!(!target.exists());
    assert!(!fs::read_dir(&fixture.root).unwrap().any(|entry| {
        entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".fliuno-backup-")
    }));

    let output = create(&["--progress"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let phases: Vec<_> = String::from_utf8(output.stderr)
        .unwrap()
        .lines()
        .map(|line| {
            serde_json::from_str::<Value>(line).unwrap()["phase"]
                .as_str()
                .unwrap()
                .to_owned()
        })
        .collect();
    assert_eq!(phases.first().unwrap(), "copying");
    assert_eq!(&phases[phases.len() - 2..], ["validating", "publishing"]);
    let created: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(created["manifest"]["taskCount"], 1);
    assert_eq!(
        kernel.get_task(&undispatched.id).unwrap().status,
        TaskStatus::Running
    );

    let rotate = |max_bytes: &str| {
        fixture
            .command()
            .arg("rotate")
            .arg(&source)
            .arg(&fixture.root)
            .args(["1", max_bytes])
            .output()
            .unwrap()
    };
    let output = rotate("100000000");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let first: Value = serde_json::from_slice(&output.stdout).unwrap();
    let first_path = PathBuf::from(first["rotation"]["path"].as_str().unwrap());
    reject(rotate("1"), "exceeds the configured total backup capacity");
    assert!(first_path.is_dir());
    let output = rotate("100000000");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let rotated: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(rotated["rotation"]["removedBackups"], 1);
    assert!(!first_path.exists());
    assert!(target.is_dir());
    assert!(fixture.backup.is_dir());

    let output = fixture
        .command()
        .arg("list")
        .arg(&fixture.root)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let listed: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(listed["backups"].as_array().unwrap().len(), 3);
}
