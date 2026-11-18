//! Small, durable identity ledger, separate from removable per-terminal spools.
//! Tombstones never expire: reclaiming output cannot authorize another launch.
use crate::protocol::{Output, READ_LIMIT, Snapshot};
use fliuno_protocol::{ObjectPage, TerminalArchive};
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use std::{
    fs::{self, File, OpenOptions},
    path::Path,
    time::Duration,
};

pub(crate) const MAX_ARCHIVES: i64 = 100_000;
const DATABASE: &str = "archive.db";
const MARKER: &str = "archive.version";
const FORMAT: &[u8] = b"fliuno-terminal-archive-v1\n";

pub(crate) fn valid_id(id: &str) -> Result<(), String> {
    if id.len() != 64
        || !id
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err("invalid terminal id".into());
    }
    Ok(())
}

pub(crate) fn lock(root: &Path) -> Result<File, String> {
    let path = root.join("registry.lock");
    regular_or_missing(&path)?;
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)
        .map_err(|_| "cannot open terminal registry lock")?;
    lock.try_lock()
        .map_err(|_| "terminal registry busy; no process started")?;
    Ok(lock)
}

pub(crate) fn regular_or_missing(path: &Path) -> Result<(), String> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.is_file() && !reparse(&meta) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        _ => Err("terminal registry path must be a regular file".into()),
    }
}
pub(crate) fn reparse(meta: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        meta.file_attributes() & 0x400 != 0
    }
    #[cfg(not(windows))]
    {
        meta.file_type().is_symlink()
    }
}

pub(crate) struct Registry {
    db: Connection,
}
#[derive(Debug)]
pub(crate) enum CommitError {
    Rejected(String),
    Uncertain(String),
}
impl From<&str> for CommitError {
    fn from(value: &str) -> Self {
        Self::Rejected(value.into())
    }
}
impl From<String> for CommitError {
    fn from(value: String) -> Self {
        Self::Rejected(value)
    }
}
impl Registry {
    /// Only the host initializes the ledger, while holding registry.lock.
    /// Workers merely read it and refuse a retired identity before PTY launch.
    pub fn initialize(root: &Path) -> Result<Self, String> {
        let initialized = match fs::symlink_metadata(root.join(MARKER)) {
            Ok(metadata) => {
                if !metadata.is_file()
                    || reparse(&metadata)
                    || metadata.len() != FORMAT.len() as u64
                    || fs::read(root.join(MARKER))
                        .map_err(|_| "cannot read terminal archive marker")?
                        != FORMAT
                {
                    return Err("invalid terminal archive marker".into());
                }
                true
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
            Err(_) => return Err("cannot inspect terminal archive marker".into()),
        };
        if initialized && !root.join(DATABASE).is_file() {
            return Err(
                "terminal archive ledger is missing; refusing to forget reserved identities".into(),
            );
        }
        regular_or_missing(&root.join(DATABASE))?;
        let mut db = Connection::open_with_flags(
            root.join(DATABASE),
            OpenFlags::default() | OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )
        .map_err(|_| "cannot open terminal archive ledger")?;
        configure(&db)?;
        db.execute_batch("PRAGMA journal_mode=DELETE; PRAGMA synchronous=EXTRA;")
            .map_err(|_| "cannot configure terminal archive durability")?;
        let tx = db
            .transaction()
            .map_err(|_| "cannot initialize terminal archive ledger")?;
        let version: u32 = tx
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .map_err(|_| "invalid terminal archive version")?;
        if initialized && version == 0 {
            return Err(
                "terminal archive ledger was reset; refusing to forget reserved identities".into(),
            );
        }
        match version {
            0 => tx.execute_batch("CREATE TABLE archives(sequence INTEGER PRIMARY KEY, id TEXT UNIQUE NOT NULL, session TEXT NOT NULL, snapshot TEXT NOT NULL, cleanup_pending INTEGER NOT NULL CHECK(cleanup_pending IN (0,1)), cleanup_attempt INTEGER NOT NULL DEFAULT 0); CREATE INDEX archives_session ON archives(session,sequence); CREATE INDEX archives_cleanup ON archives(cleanup_pending,cleanup_attempt,sequence); PRAGMA user_version=1;")
                .map_err(|_| "cannot create terminal archive ledger")?,
            1 => {},
            _ => return Err("unsupported terminal archive version".into()),
        }
        tx.commit()
            .map_err(|_| "cannot commit terminal archive initialization")?;
        if !initialized {
            use std::io::Write;
            let mut marker = OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(root.join(MARKER))
                .map_err(|_| "cannot reserve terminal archive marker")?;
            marker
                .write_all(FORMAT)
                .and_then(|_| marker.sync_all())
                .map_err(|_| "cannot persist terminal archive marker")?;
        }
        Ok(Self { db })
    }
    pub fn open(root: &Path, writable: bool) -> Result<Self, String> {
        regular_or_missing(&root.join(DATABASE))?;
        let flags = if writable {
            OpenFlags::SQLITE_OPEN_READ_WRITE
        } else {
            OpenFlags::SQLITE_OPEN_READ_ONLY
        };
        let db = Connection::open_with_flags(
            root.join(DATABASE),
            flags | OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )
        .map_err(|_| "terminal archive ledger unavailable")?;
        configure(&db)?;
        let version: u32 = db
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .map_err(|_| "invalid terminal archive version")?;
        if version != 1 {
            return Err("unsupported terminal archive version".into());
        }
        if writable {
            db.execute_batch("PRAGMA synchronous=EXTRA;")
                .map_err(|_| "cannot configure terminal archive durability")?;
        }
        Ok(Self { db })
    }
    pub fn get(&self, id: &str) -> Result<Option<TerminalArchive>, String> {
        valid_id(id)?;
        let record = self
            .db
            .prepare_cached("SELECT snapshot,cleanup_pending FROM archives WHERE id=?1")
            .map_err(|_| "cannot read terminal archive")?
            .query_row([id], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, bool>(1)?))
            })
            .optional()
            .map_err(|_| "cannot read terminal archive")?;
        record
            .map(|(text, pending)| {
                let record = decode(&text, pending)?;
                if record.snapshot.terminal_id != id {
                    return Err("terminal archive identity mismatch".into());
                }
                Ok(record)
            })
            .transpose()
    }
    /// Caller owns both registry.lock and owner.lock. Commit BEFORE unlinking
    /// any spool, endpoint or operation evidence. A crash cannot reopen the ID.
    pub fn insert(&mut self, snapshot: &Snapshot) -> Result<(), CommitError> {
        valid_id(&snapshot.terminal_id)?;
        if snapshot.archived_at_unix_ms.is_none()
            || snapshot.start_offset != snapshot.end_offset
            || snapshot.supervisor_connected
        {
            return Err("invalid terminal tombstone".into());
        }
        let tx = self
            .db
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(|_| "cannot reserve terminal tombstone")?;
        let count: i64 = tx
            .query_row("SELECT COUNT(*) FROM archives", [], |row| row.get(0))
            .map_err(|_| "cannot inspect terminal archive capacity")?;
        if count >= MAX_ARCHIVES {
            return Err(
                "terminal archive ledger capacity reached; identities cannot be forgotten".into(),
            );
        }
        tx.execute(
            "INSERT INTO archives(id,session,snapshot,cleanup_pending) VALUES(?1,?2,?3,1)",
            params![
                snapshot.terminal_id,
                snapshot.session_id,
                serde_json::to_string(snapshot).map_err(|_| "invalid terminal tombstone")?
            ],
        )
        .map_err(|_| "cannot persist terminal tombstone")?;
        tx.commit().map_err(|_| {
            CommitError::Uncertain(
                "terminal tombstone commit outcome unknown; inspect this terminal before retrying"
                    .into(),
            )
        })?;
        Ok(())
    }
    pub fn cleaned(&self, id: &str) -> Result<(), String> {
        self.db
            .execute("UPDATE archives SET cleanup_pending=0 WHERE id=?1", [id])
            .map_err(|_| "cannot save terminal archive cleanup")?;
        Ok(())
    }
    pub fn pending(&self, limit: u32) -> Result<Vec<String>, String> {
        let mut statement = self
            .db
            .prepare("SELECT id FROM archives WHERE cleanup_pending=1 ORDER BY cleanup_attempt,sequence LIMIT ?1")
            .map_err(|_| "cannot inspect pending terminal archives")?;
        statement
            .query_map([limit], |row| row.get(0))
            .map_err(|_| "cannot inspect pending terminal archives")?
            .collect::<Result<_, _>>()
            .map_err(|_| "invalid pending terminal archive".into())
    }
    pub fn deferred(&self, id: &str) -> Result<(), String> {
        self.db.execute("UPDATE archives SET cleanup_attempt=(SELECT COALESCE(MAX(cleanup_attempt),0)+1 FROM archives WHERE cleanup_pending=1) WHERE id=?1", [id])
            .map_err(|_| "cannot defer terminal archive cleanup")?;
        Ok(())
    }
    pub fn list(
        &self,
        session: &str,
        after: u64,
        limit: u32,
    ) -> Result<ObjectPage<TerminalArchive>, String> {
        if after > i64::MAX as u64 || !(1..=100).contains(&limit) {
            return Err("invalid terminal archive page".into());
        }
        let mut statement = self.db.prepare("SELECT sequence,snapshot,cleanup_pending FROM archives WHERE session=?1 AND sequence>?2 ORDER BY sequence LIMIT ?3")
            .map_err(|_| "cannot list terminal archives")?;
        let rows = statement
            .query_map(params![session, after as i64, limit + 1], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, bool>(2)?,
                ))
            })
            .map_err(|_| "cannot list terminal archives")?;
        let mut items = Vec::new();
        let mut next_after = after;
        let mut has_more = false;
        for row in rows {
            let (sequence, snapshot, pending) =
                row.map_err(|_| "cannot read terminal archive page")?;
            if items.len() == limit as usize {
                has_more = true;
                break;
            }
            let record = decode(&snapshot, pending)?;
            if record.snapshot.session_id != session {
                return Err("terminal archive ownership mismatch".into());
            }
            items.push(record);
            next_after =
                u64::try_from(sequence).map_err(|_| "invalid terminal archive sequence")?;
        }
        Ok(ObjectPage {
            items,
            next_after,
            has_more,
        })
    }
}
fn configure(db: &Connection) -> Result<(), String> {
    db.busy_timeout(Duration::from_millis(50))
        .map_err(|_| "cannot bound terminal archive IO".into())
}
fn decode(text: &str, cleanup_pending: bool) -> Result<TerminalArchive, String> {
    let snapshot: Snapshot = serde_json::from_str(text).map_err(|_| "invalid terminal archive")?;
    valid_id(&snapshot.terminal_id)?;
    if snapshot.archived_at_unix_ms.is_none()
        || snapshot.supervisor_connected
        || snapshot.start_offset != snapshot.end_offset
        || snapshot.end_offset > i64::MAX as u64
    {
        return Err("invalid terminal tombstone".into());
    }
    Ok(TerminalArchive {
        snapshot,
        cleanup_pending,
    })
}

pub(crate) fn output(snapshot: Snapshot, after: u64, limit: usize) -> Result<Output, String> {
    if !(1..=READ_LIMIT).contains(&limit) || after > snapshot.end_offset {
        return Err("invalid terminal output cursor or limit".into());
    }
    Ok(Output {
        from: snapshot.end_offset,
        next: snapshot.end_offset,
        gap: after < snapshot.end_offset,
        data_base64: String::new(),
        snapshot,
    })
}
