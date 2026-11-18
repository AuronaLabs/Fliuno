//! Retire a stopped supervisor's identity before reclaiming any of its files.
//! Owner locks, not saved PIDs or a stale endpoint, decide cleanup eligibility.
mod batch;
pub(crate) mod retention;
use crate::{
    Service,
    protocol::*,
    registry::{self, Registry},
    spool,
};
use fliuno_core::ToolError;
use fliuno_protocol::{ObjectPage, TerminalArchive};
use std::{
    fs::{self, File, OpenOptions},
    path::Path,
};

impl Service {
    /// Trusted client retention operation. The model-facing adapter requires
    /// its own durable approval. A live supervisor is never killed implicitly.
    pub fn archive(&self, session: &str, id: &str) -> Result<TerminalArchive, ToolError> {
        registry::valid_id(id)?;
        let _registry_lock = registry::lock(&self.root)?;
        let mut ledger = Registry::open(&self.root, true)?;
        if let Some(mut record) = ledger.get(id)? {
            owns(&record.snapshot, session)?;
            if record.cleanup_pending && cleanup(&self.root, id) {
                record.cleanup_pending = ledger.cleaned(id).is_err();
            }
            return Ok(record);
        }
        let directory = self.directory(id)?;
        owns(&archive_snapshot(&directory, id)?, session)?;
        // Keep this handle across snapshot, tombstone commit and deletion.
        // Even a delayed worker cannot pass its owner lock and ledger check.
        let owner = owner_lock(&directory)?;
        let snapshot = archive_snapshot(&directory, id)?;
        owns(&snapshot, session)?;
        archive_owned(&mut ledger, &directory, owner, snapshot, now_ms())
    }

    pub fn archives(
        &self,
        session: &str,
        after: u64,
        limit: u32,
    ) -> Result<ObjectPage<TerminalArchive>, String> {
        Registry::open(&self.root, false)?.list(session, after, limit)
    }
}

fn archive_snapshot(directory: &Path, id: &str) -> Result<Snapshot, String> {
    let snapshot = spool::saved_snapshot(directory)?;
    if snapshot.terminal_id != id || snapshot.archived_at_unix_ms.is_some() {
        return Err("terminal archive identity does not match spool".into());
    }
    Ok(snapshot)
}

fn archive_owned(
    ledger: &mut Registry,
    directory: &Path,
    owner: File,
    mut snapshot: Snapshot,
    now: u64,
) -> Result<TerminalArchive, ToolError> {
    snapshot.archived_at_unix_ms = Some(now);
    snapshot.start_offset = snapshot.end_offset;
    ledger.insert(&snapshot).map_err(|error| match error {
        registry::CommitError::Rejected(message) => ToolError::from(message),
        registry::CommitError::Uncertain(message) => ToolError::OutcomeUnknownWithOutput {
            message,
            output: serde_json::json!({"terminalId":snapshot.terminal_id,"sessionId":snapshot.session_id,"phase":"archive"}),
        },
    })?;
    let cleanup_pending =
        !clean_owned(directory, owner) || ledger.cleaned(&snapshot.terminal_id).is_err();
    Ok(TerminalArchive {
        snapshot,
        cleanup_pending,
    })
}

fn owns(snapshot: &Snapshot, session: &str) -> Result<(), String> {
    if snapshot.session_id != session {
        return Err("terminal belongs to another session".into());
    }
    Ok(())
}

fn owner_lock(directory: &Path) -> Result<File, String> {
    let meta = fs::symlink_metadata(directory).map_err(|_| "terminal not found")?;
    if !meta.is_dir() || registry::reparse(&meta) {
        return Err("invalid terminal directory".into());
    }
    let path = directory.join("owner.lock");
    registry::regular_or_missing(&path)?;
    let owner = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)
        .map_err(|_| "cannot lock terminal for archive")?;
    owner
        .try_lock()
        .map_err(|_| "terminal supervisor still owns its record; release it before archiving")?;
    Ok(owner)
}

/// No recursive deletion. Unexpected entries, links and directory redirections
/// leave cleanup pending instead of widening what an archive request can erase.
fn clean_owned(directory: &Path, owner: File) -> bool {
    let Ok(entries) = fs::read_dir(directory) else {
        return false;
    };
    let mut files = Vec::new();
    for entry in entries.take(17) {
        let Ok(entry) = entry else { return false };
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            return false;
        };
        if !matches!(
            name,
            "owner.lock"
                | "endpoint.json"
                | "output.db"
                | "output.db-journal"
                | "output.db-wal"
                | "output.db-shm"
                | "ipc.sock"
        ) {
            return false;
        }
        let Ok(meta) = fs::symlink_metadata(entry.path()) else {
            return false;
        };
        if registry::reparse(&meta) {
            return false;
        }
        #[cfg(unix)]
        let socket = {
            use std::os::unix::fs::FileTypeExt;
            name == "ipc.sock" && meta.file_type().is_socket()
        };
        #[cfg(not(unix))]
        let socket = false;
        if !meta.is_file() && !socket {
            return false;
        }
        files.push(entry.path());
    }
    if files.len() > 16 {
        return false;
    }
    for file in files {
        if fs::remove_file(file).is_err() {
            return false;
        }
    }
    // Windows may defer deleting a locked file until its handle closes. The
    // tombstone is already committed; a late worker must consult it and stop.
    drop(owner);
    fs::remove_dir(directory).is_ok()
}

pub(crate) fn cleanup(root: &Path, id: &str) -> bool {
    if registry::valid_id(id).is_err() {
        return false;
    }
    let directory = root.join(id);
    match fs::symlink_metadata(&directory) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => true,
        Err(_) => false,
        Ok(_) => match owner_lock(&directory) {
            Ok(owner) => clean_owned(&directory, owner),
            Err(_) => false,
        },
    }
}

/// Only already-committed tombstones participate in startup/capacity cleanup.
/// The caller holds registry.lock. A bounded batch keeps startup predictable.
pub(crate) fn recover(root: &Path, ledger: &Registry) -> Result<(), String> {
    for id in ledger.pending(8)? {
        if cleanup(root, &id) {
            ledger.cleaned(&id)?
        } else {
            ledger.deferred(&id)?
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "archive/tests.rs"]
mod tests;
