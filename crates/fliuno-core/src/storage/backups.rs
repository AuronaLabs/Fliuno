use super::*;
use rusqlite::{
    OpenFlags,
    backup::{Backup, StepResult},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    io::{BufWriter, Read, Write},
    path::{Path, PathBuf},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use uuid::Uuid;

const BACKUP_FORMAT_VERSION: u32 = 1;
const DATABASE_FILE: &str = "database.sqlite";
const MANIFEST_FILE: &str = "manifest.json";
const ARTIFACTS_FILE: &str = "artifacts.sha256";
const ROTATING_BACKUP_PREFIX: &str = "fliuno-backup-";
const MAX_BACKUP_RETRIES: Duration = Duration::from_secs(15 * 60);

/// Bounded retention policy for timestamped backups in one directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BackupRetention {
    pub keep_count: u32,
    pub max_total_bytes: u64,
    pub max_age_ms: Option<u64>,
}

/// Summary returned after a backup has been committed and retention applied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackupRotationResult {
    pub manifest: BackupManifest,
    pub retained_backups: u32,
    pub retained_bytes: u64,
    pub removed_backups: u32,
}

/// Content and integrity metadata for one consistent database snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BackupManifest {
    pub format_version: u32,
    pub fliuno_version: String,
    pub schema_version: u32,
    pub created_at_ms: u64,
    pub database_bytes: u64,
    pub database_sha256: String,
    pub artifact_manifest_bytes: u64,
    pub artifact_manifest_sha256: String,
    pub event_through_sequence: u64,
    pub task_count: u64,
    pub artifact_count: u64,
    pub artifact_bytes: u64,
}

pub(super) fn create(db: &Connection, destination: &Path) -> Result<BackupManifest> {
    let (parent, target) = resolve_target(destination)?;
    if target.exists() {
        return Err(Error::Conflict("backup destination already exists".into()));
    }
    let staging = parent.join(format!(".fliuno-backup-{}.tmp", Uuid::new_v4()));
    create_private_dir(&staging)?;
    let result = create_staged(db, &staging).and_then(|manifest| {
        fs::rename(&staging, &target)?;
        sync_directory(&parent)?;
        Ok(manifest)
    });
    if result.is_err() {
        let _ = fs::remove_dir_all(&staging);
    }
    result
}

pub(super) fn create_rotating(
    db: &Connection,
    directory: &Path,
    policy: BackupRetention,
) -> Result<BackupRotationResult> {
    if !(1..=256).contains(&policy.keep_count) || policy.max_total_bytes == 0 {
        return Err(Error::Invalid(
            "backup retention must keep 1..256 entries and allow at least one byte".into(),
        ));
    }
    fs::create_dir_all(directory)?;
    let root = fs::canonicalize(directory)?;
    let name = format!("{ROTATING_BACKUP_PREFIX}{}-{}", now_ms(), Uuid::new_v4());
    let created_path = root.join(name);
    let manifest = create(db, &created_path)?;
    let new_bytes = backup_size(&created_path, &manifest)?;
    if new_bytes > policy.max_total_bytes {
        fs::remove_dir_all(&created_path)?;
        return Err(Error::Invalid(
            "new backup exceeds the configured total backup capacity".into(),
        ));
    }

    let mut entries = list_valid_backups(&root)?;
    entries.sort_by(|left, right| {
        left.manifest
            .created_at_ms
            .cmp(&right.manifest.created_at_ms)
            .then_with(|| left.path.cmp(&right.path))
    });
    let mut retained = entries;
    let mut retained_bytes: u64 = retained.iter().map(|entry| entry.bytes).sum();
    let mut removed_backups = 0_u32;
    if let Some(max_age_ms) = policy.max_age_ms {
        let oldest_allowed = now_ms().saturating_sub(max_age_ms);
        let expired: Vec<_> = retained
            .iter()
            .filter(|entry| {
                entry.path != created_path && entry.manifest.created_at_ms < oldest_allowed
            })
            .map(|entry| entry.path.clone())
            .collect();
        for path in expired {
            if let Some(index) = retained.iter().position(|entry| entry.path == path) {
                let entry = retained.remove(index);
                fs::remove_dir_all(&entry.path)?;
                retained_bytes = retained_bytes.saturating_sub(entry.bytes);
                removed_backups += 1;
            }
        }
    }
    while retained.len() > policy.keep_count as usize || retained_bytes > policy.max_total_bytes {
        let oldest_existing = retained
            .iter()
            .position(|entry| entry.path != created_path)
            .ok_or_else(|| {
                Error::Invalid("backup capacity cannot retain the new snapshot".into())
            })?;
        let entry = retained.remove(oldest_existing);
        fs::remove_dir_all(&entry.path)?;
        retained_bytes = retained_bytes.saturating_sub(entry.bytes);
        removed_backups += 1;
    }
    Ok(BackupRotationResult {
        manifest,
        retained_backups: retained.len() as u32,
        retained_bytes,
        removed_backups,
    })
}

fn create_staged(db: &Connection, staging: &Path) -> Result<BackupManifest> {
    let database = staging.join(DATABASE_FILE);
    let mut target = Connection::open(&database)?;
    target.pragma_update(None, "journal_mode", "DELETE")?;
    {
        let backup = Backup::new(db, &mut target)?;
        let started = Instant::now();
        loop {
            if started.elapsed() >= MAX_BACKUP_RETRIES {
                return Err(Error::Conflict(
                    "SQLite backup exceeded its time limit".into(),
                ));
            }
            match backup.step(256)? {
                StepResult::Done => break,
                StepResult::More => {}
                StepResult::Busy | StepResult::Locked => {
                    if started.elapsed() >= MAX_BACKUP_RETRIES {
                        return Err(Error::Conflict(
                            "SQLite backup could not acquire a consistent snapshot in time".into(),
                        ));
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
                _ => {
                    return Err(Error::Conflict(
                        "SQLite backup returned an unknown progress state".into(),
                    ));
                }
            }
        }
    }
    target.close().map_err(|(_, error)| error)?;

    let snapshot = Connection::open_with_flags(
        &database,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    let artifacts_path = staging.join(ARTIFACTS_FILE);
    let artifacts_file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&artifacts_path)?;
    let mut artifacts_writer = HashingWriter::new(BufWriter::new(artifacts_file));
    write_artifact_manifest(&snapshot, &mut artifacts_writer)?;
    artifacts_writer.flush()?;
    let (artifact_manifest_bytes, artifact_manifest_sha256) = artifacts_writer.finish();
    drop(snapshot);

    let mut manifest = inspect_database(&database)?;
    if manifest.artifact_manifest_bytes != artifact_manifest_bytes
        || manifest.artifact_manifest_sha256 != artifact_manifest_sha256
    {
        return Err(Error::Conflict(
            "artifact manifest changed while the backup was being written".into(),
        ));
    }
    manifest.created_at_ms = now_ms();
    manifest.database_bytes = fs::metadata(&database)?.len();
    manifest.database_sha256 = sha256_file(&database)?;

    let manifest_path = staging.join(MANIFEST_FILE);
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(manifest_path)?;
    serde_json::to_writer(&mut file, &manifest)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    OpenOptions::new()
        .read(true)
        .write(true)
        .open(&artifacts_path)?
        .sync_all()?;
    OpenOptions::new()
        .read(true)
        .write(true)
        .open(database)?
        .sync_all()?;
    Ok(manifest)
}

pub(super) fn verify(directory: &Path) -> Result<BackupManifest> {
    let manifest_path = directory.join(MANIFEST_FILE);
    let manifest: BackupManifest = serde_json::from_slice(&fs::read(&manifest_path)?)?;
    if manifest.format_version != BACKUP_FORMAT_VERSION {
        return Err(Error::Invalid("unsupported Fliuno backup format".into()));
    }
    let database = directory.join(DATABASE_FILE);
    let actual_bytes = fs::metadata(&database)?.len();
    let actual_hash = sha256_file(&database)?;
    if actual_bytes != manifest.database_bytes || actual_hash != manifest.database_sha256 {
        return Err(Error::Conflict(
            "backup database does not match its manifest".into(),
        ));
    }
    let actual = inspect_database(&database)?;
    let artifact_manifest_path = directory.join(ARTIFACTS_FILE);
    let artifact_manifest_file_bytes = fs::metadata(&artifact_manifest_path)?.len();
    let artifact_manifest_file_hash = sha256_file(&artifact_manifest_path)?;
    if actual.schema_version != manifest.schema_version
        || actual.event_through_sequence != manifest.event_through_sequence
        || actual.task_count != manifest.task_count
        || actual.artifact_count != manifest.artifact_count
        || actual.artifact_bytes != manifest.artifact_bytes
        || actual.artifact_manifest_bytes != manifest.artifact_manifest_bytes
        || actual.artifact_manifest_sha256 != manifest.artifact_manifest_sha256
        || artifact_manifest_file_bytes != manifest.artifact_manifest_bytes
        || artifact_manifest_file_hash != manifest.artifact_manifest_sha256
    {
        return Err(Error::Conflict(
            "backup database contents do not match its manifest".into(),
        ));
    }
    Ok(manifest)
}

pub(super) fn restore(directory: &Path, destination: &Path) -> Result<BackupManifest> {
    let manifest = verify(directory)?;
    let (parent, target) = resolve_target(destination)?;
    if target.exists() {
        return Err(Error::Conflict(
            "restore destination already exists; restore to a new database path".into(),
        ));
    }
    let staging = parent.join(format!(".fliuno-restore-{}.tmp", Uuid::new_v4()));
    let result = (|| {
        let mut lock_path = target.as_os_str().to_os_string();
        lock_path.push(".lock");
        let lock_path = PathBuf::from(lock_path);
        let _lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(lock_path)?;
        _lock.try_lock().map_err(|error| {
            Error::Conflict(format!("cannot lock restore destination: {error}"))
        })?;

        let source = directory.join(DATABASE_FILE);
        fs::copy(source, &staging)?;
        if fs::metadata(&staging)?.len() != manifest.database_bytes
            || sha256_file(&staging)? != manifest.database_sha256
        {
            return Err(Error::Conflict(
                "restored database copy failed its digest check".into(),
            ));
        }
        OpenOptions::new()
            .read(true)
            .write(true)
            .open(&staging)?
            .sync_all()?;
        fs::rename(&staging, &target)?;
        sync_directory(&parent)?;
        if let Err(error) = verify_restored(&target, &manifest) {
            let _ = fs::remove_file(&target);
            return Err(error);
        }
        Ok(manifest)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&staging);
    }
    result
}

pub(super) fn verify_restored(path: &Path, expected: &BackupManifest) -> Result<()> {
    if fs::metadata(path)?.len() != expected.database_bytes
        || sha256_file(path)? != expected.database_sha256
    {
        return Err(Error::Conflict(
            "restored database does not match its backup manifest".into(),
        ));
    }
    let actual = inspect_database(path)?;
    if actual.schema_version != expected.schema_version
        || actual.event_through_sequence != expected.event_through_sequence
        || actual.task_count != expected.task_count
        || actual.artifact_count != expected.artifact_count
        || actual.artifact_bytes != expected.artifact_bytes
        || actual.artifact_manifest_bytes != expected.artifact_manifest_bytes
        || actual.artifact_manifest_sha256 != expected.artifact_manifest_sha256
    {
        return Err(Error::Conflict(
            "restored database failed structural validation".into(),
        ));
    }
    Ok(())
}

fn resolve_target(path: &Path) -> Result<(PathBuf, PathBuf)> {
    let name = path
        .file_name()
        .filter(|name| !name.is_empty())
        .ok_or_else(|| {
            Error::Invalid("backup destination must name a directory or database".into())
        })?;
    let requested_parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    fs::create_dir_all(requested_parent)?;
    let parent = fs::canonicalize(requested_parent)?;
    Ok((parent.clone(), parent.join(name)))
}

fn inspect_database(path: &Path) -> Result<BackupManifest> {
    let db = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    let integrity: String = db.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
    if integrity != "ok" {
        return Err(Error::Conflict(
            "backup database failed integrity_check".into(),
        ));
    }
    let mut foreign_keys = db.prepare("PRAGMA foreign_key_check")?;
    if foreign_keys.query([])?.next()?.is_some() {
        return Err(Error::Conflict(
            "backup database contains foreign-key violations".into(),
        ));
    }
    let schema_version: u32 = db.pragma_query_value(None, "user_version", |row| row.get(0))?;
    if schema_version != SCHEMA_VERSION {
        return Err(Error::Conflict(
            "backup must use the current database schema".into(),
        ));
    }
    let event_through_sequence = scalar_u64(&db, "SELECT COALESCE(MAX(sequence),0) FROM events")?;
    let task_count = scalar_u64(&db, "SELECT COUNT(*) FROM tasks")?;
    let artifact_count = scalar_u64(&db, "SELECT COUNT(*) FROM artifacts")?;
    let artifact_bytes = scalar_u64(
        &db,
        "SELECT COALESCE(SUM(length(body)),0) FROM artifact_bodies",
    )?;
    let (artifact_manifest_bytes, artifact_manifest_sha256) = {
        let mut writer = HashingWriter::new(std::io::sink());
        write_artifact_manifest(&db, &mut writer)?;
        writer.finish()
    };
    Ok(BackupManifest {
        format_version: BACKUP_FORMAT_VERSION,
        fliuno_version: env!("CARGO_PKG_VERSION").into(),
        schema_version,
        created_at_ms: 0,
        database_bytes: 0,
        database_sha256: String::new(),
        artifact_manifest_bytes,
        artifact_manifest_sha256,
        event_through_sequence,
        task_count,
        artifact_count,
        artifact_bytes,
    })
}

fn scalar_u64(db: &Connection, sql: &str) -> Result<u64> {
    let value: i64 = db.query_row(sql, [], |row| row.get(0))?;
    u64::try_from(value).map_err(|_| Error::Conflict("database count is negative".into()))
}

fn sha256_file(path: &Path) -> Result<String> {
    let mut file = File::open(path)?;
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        digest.update(&buffer[..count]);
    }
    Ok(format!("{:x}", digest.finalize()))
}

struct BackupEntry {
    path: PathBuf,
    manifest: BackupManifest,
    bytes: u64,
}

fn list_valid_backups(directory: &Path) -> Result<Vec<BackupEntry>> {
    let mut backups = Vec::new();
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir()
            || !entry
                .file_name()
                .to_string_lossy()
                .starts_with(ROTATING_BACKUP_PREFIX)
        {
            continue;
        }
        let path = entry.path();
        let Ok(manifest) = verify(&path) else {
            continue;
        };
        let bytes = backup_size(&path, &manifest)?;
        backups.push(BackupEntry {
            path,
            manifest,
            bytes,
        });
    }
    Ok(backups)
}

fn backup_size(directory: &Path, manifest: &BackupManifest) -> Result<u64> {
    let manifest_bytes = fs::metadata(directory.join(MANIFEST_FILE))?.len();
    manifest
        .database_bytes
        .checked_add(manifest.artifact_manifest_bytes)
        .and_then(|size| size.checked_add(manifest_bytes))
        .ok_or_else(|| Error::Invalid("backup size overflow".into()))
}

struct HashingWriter<W> {
    inner: W,
    digest: Sha256,
    bytes: u64,
}

impl<W> HashingWriter<W> {
    fn new(inner: W) -> Self {
        Self {
            inner,
            digest: Sha256::new(),
            bytes: 0,
        }
    }

    fn finish(self) -> (u64, String) {
        (self.bytes, format!("{:x}", self.digest.finalize()))
    }
}

impl<W: Write> Write for HashingWriter<W> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.inner.write_all(bytes)?;
        self.digest.update(bytes);
        self.bytes = self.bytes.saturating_add(bytes.len() as u64);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

fn write_artifact_manifest(db: &Connection, output: &mut impl Write) -> Result<()> {
    writeln!(output, "fliuno-artifacts-v1")?;
    let mut after = 0_i64;
    loop {
        let page: Vec<(i64, String, String, i64)> = {
            let mut statement = db.prepare(
                "SELECT b.rowid,a.id,a.task_id,length(b.body)
                 FROM artifact_bodies b JOIN artifacts a ON a.id=b.artifact_id
                 WHERE b.rowid>?1 ORDER BY b.rowid LIMIT 128",
            )?;
            statement
                .query_map([after], |row| {
                    Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
                })?
                .collect::<std::result::Result<_, _>>()?
        };
        if page.is_empty() {
            break;
        }
        for (rowid, artifact_id, task_id, byte_count) in page {
            after = rowid;
            let blob = db.blob_open("main", "artifact_bodies", "body", rowid, true)?;
            let mut digest = Sha256::new();
            let mut buffer = [0_u8; 64 * 1024];
            let mut offset = 0_usize;
            while offset < blob.len() {
                let count = buffer.len().min(blob.len() - offset);
                blob.read_at_exact(&mut buffer[..count], offset)?;
                digest.update(&buffer[..count]);
                offset += count;
            }
            writeln!(
                output,
                "{artifact_id}\t{task_id}\t{byte_count}\t{:x}",
                digest.finalize()
            )?;
        }
    }
    Ok(())
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u64::MAX as u128) as u64
}

fn create_private_dir(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        let mut builder = fs::DirBuilder::new();
        builder.mode(0o700).create(path)?;
    }
    #[cfg(not(unix))]
    fs::create_dir(path)?;
    Ok(())
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> Result<()> {
    File::open(path)?.sync_all()?;
    Ok(())
}

#[cfg(not(unix))]
fn sync_directory(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ApprovalDecision, DemoProvider};

    fn temp_dir(label: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!("fliuno-{label}-{}", Uuid::new_v4()));
        fs::create_dir(&path).unwrap();
        path
    }

    #[test]
    fn backup_verifies_restores_tasks_and_binary_artifacts() {
        let root = temp_dir("backup-roundtrip");
        let mut kernel = Kernel::open(root.join("source.db"), Box::new(DemoProvider)).unwrap();
        let workspace = kernel
            .open_workspace(std::env::current_dir().unwrap().to_str().unwrap())
            .unwrap();
        let session = kernel.create_session(&workspace.id).unwrap();
        let waiting = kernel.start_task(&session.id, "backup payload").unwrap();
        let approval = waiting.pending_approval.unwrap();
        let completed = kernel
            .respond_approval(&waiting.id, &approval.id, ApprovalDecision::AllowOnce)
            .unwrap();
        assert_eq!(completed.status, TaskStatus::Completed);
        let binary = [0_u8, 1, 127, 255];
        kernel
            .db
            .execute(
                "INSERT INTO artifacts(id,task_id,data) VALUES(?1,?2,?3)",
                rusqlite::params![
                    "artifact_binary_test",
                    completed.id,
                    serde_json::json!({
                        "title":"binary",
                        "mediaType":"application/octet-stream",
                        "contentBytes":binary.len(),
                        "binary":true,
                        "sha256":format!("{:x}", Sha256::digest(binary)),
                    })
                    .to_string(),
                ],
            )
            .unwrap();
        kernel
            .db
            .execute(
                "INSERT INTO artifact_bodies(artifact_id,body) VALUES(?1,?2)",
                rusqlite::params!["artifact_binary_test", binary.as_slice()],
            )
            .unwrap();

        let backup_dir = root.join("backup");
        let manifest = kernel.create_backup(&backup_dir).unwrap();
        assert_eq!(manifest.task_count, 1);
        assert_eq!(manifest.artifact_count, 2);
        assert_eq!(
            manifest.artifact_bytes,
            "backup payload".len() as u64 + binary.len() as u64
        );
        assert_eq!(Kernel::verify_backup(&backup_dir).unwrap(), manifest);
        assert!(Kernel::restore_backup(&backup_dir, root.join("source.db")).is_err());

        let restored_path = root.join("restored.db");
        let restored_manifest = Kernel::restore_backup(&backup_dir, &restored_path).unwrap();
        assert_eq!(restored_manifest, manifest);
        Kernel::verify_restored_database(&restored_path, &manifest).unwrap();
        let restored = Kernel::open(&restored_path, Box::new(DemoProvider)).unwrap();
        assert_eq!(
            restored.get_task(&completed.id).unwrap().status,
            TaskStatus::Completed
        );
        let artifacts = restored.list_artifacts(&completed.id).unwrap();
        assert_eq!(artifacts.len(), 2);
        assert!(matches!(
            &artifacts[0].body,
            ArtifactBody::Text { content } if content == "backup payload"
        ));
        assert!(matches!(
            &artifacts[1].body,
            ArtifactBody::Binary { bytes } if bytes == &binary
        ));
        drop(restored);
        drop(kernel);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn backup_rejects_tampering_and_existing_destinations() {
        let root = temp_dir("backup-integrity");
        let kernel = Kernel::in_memory(Box::new(DemoProvider)).unwrap();
        let backup_dir = root.join("backup");
        kernel.create_backup(&backup_dir).unwrap();
        assert!(kernel.create_backup(&backup_dir).is_err());

        OpenOptions::new()
            .append(true)
            .open(backup_dir.join(DATABASE_FILE))
            .unwrap()
            .write_all(b"tampered")
            .unwrap();
        assert!(Kernel::verify_backup(&backup_dir).is_err());

        let artifact_manifest = root.join("backup-manifest-tampered");
        kernel.create_backup(&artifact_manifest).unwrap();
        OpenOptions::new()
            .append(true)
            .open(artifact_manifest.join(ARTIFACTS_FILE))
            .unwrap()
            .write_all(b"tampered")
            .unwrap();
        assert!(Kernel::verify_backup(&artifact_manifest).is_err());
        drop(kernel);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn backup_rotation_applies_retention_only_after_a_valid_snapshot_is_written() {
        let root = temp_dir("backup-rotation");
        let mut kernel = Kernel::open(root.join("source.db"), Box::new(DemoProvider)).unwrap();
        let workspace = kernel
            .open_workspace(std::env::current_dir().unwrap().to_str().unwrap())
            .unwrap();
        let session = kernel.create_session(&workspace.id).unwrap();
        let task = kernel.start_task(&session.id, "live task").unwrap();
        let backups = root.join("backups");
        let policy = BackupRetention {
            keep_count: 2,
            max_total_bytes: u64::MAX,
            max_age_ms: None,
        };
        kernel
            .create_backup_with_retention(&backups, policy)
            .unwrap();
        kernel
            .create_backup_with_retention(&backups, policy)
            .unwrap();
        let latest = kernel
            .create_backup_with_retention(&backups, policy)
            .unwrap();
        assert_eq!(latest.retained_backups, 2);
        assert_eq!(latest.removed_backups, 1);
        assert_eq!(list_valid_backups(&backups).unwrap().len(), 2);

        let too_small = BackupRetention {
            max_total_bytes: 1,
            ..policy
        };
        assert!(
            kernel
                .create_backup_with_retention(&backups, too_small)
                .is_err()
        );
        assert_eq!(list_valid_backups(&backups).unwrap().len(), 2);
        assert_eq!(
            kernel.get_task(&task.id).unwrap().status,
            TaskStatus::WaitingApproval
        );
        drop(kernel);
        fs::remove_dir_all(root).unwrap();
    }
}
