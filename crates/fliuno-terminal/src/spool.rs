//! Bounded durable byte log. Output and its cursor snapshot commit together.
//! Terminal escape/UTF-8 boundaries do not alter byte cursors or truncate data.
use crate::protocol::{Output, Snapshot};
use base64::Engine;
use rusqlite::{Connection, params};
use std::{path::Path, time::Duration};

// Output floods can hold the rollback journal through a slow disk flush.
// Keep independent readers bounded without interrupting the writer's commit.
const READ_BUSY_TIMEOUT: Duration = Duration::from_millis(750);

pub(crate) struct Spool {
    db: Connection,
    cap: usize,
}
pub(crate) type MutationResult = Result<serde_json::Value, String>;
impl Spool {
    pub fn create(dir: &Path, cap: usize, snapshot: &Snapshot) -> Result<Self, String> {
        // Reserve a new database before launching any supervisor. A failed
        // bootstrap therefore still has an inspectable session-owned record.
        std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(dir.join("output.db"))
            .map_err(|_| "terminal spool already exists or cannot be reserved")?;
        let db =
            Connection::open(dir.join("output.db")).map_err(|_| "cannot open terminal spool")?;
        db.execute_batch("PRAGMA journal_mode=DELETE; PRAGMA synchronous=FULL;")
            .map_err(|_| "cannot configure terminal spool")?;
        let tx = db
            .unchecked_transaction()
            .map_err(|_| "cannot begin terminal reservation")?;
        tx.execute_batch("CREATE TABLE snapshot(data TEXT NOT NULL); CREATE TABLE chunks(offset INTEGER PRIMARY KEY,data BLOB NOT NULL); CREATE TABLE operations(id TEXT PRIMARY KEY, request TEXT NOT NULL,result TEXT); PRAGMA user_version=1;").map_err(|_|"cannot initialize terminal spool")?;
        tx.execute(
            "INSERT INTO snapshot VALUES(?1)",
            [serde_json::to_string(snapshot).map_err(|_| "invalid terminal snapshot")?],
        )
        .map_err(|_| "cannot create terminal snapshot")?;
        tx.commit()
            .map_err(|_| "cannot commit terminal reservation")?;
        Ok(Self { db, cap })
    }
    /// The worker may only adopt the exact fresh reservation supplied by its
    /// host. This is not a general-purpose restart/relaunch path.
    pub fn open_reserved(dir: &Path, cap: usize, expected: &Snapshot) -> Result<Self, String> {
        let db = Connection::open_with_flags(
            dir.join("output.db"),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE,
        )
        .map_err(|_| "terminal reservation unavailable")?;
        let version: u32 = db
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .map_err(|_| "invalid terminal spool version")?;
        if version != 1
            || read_snapshot(&db)? != *expected
            || expected.status != crate::protocol::Status::Starting
            || expected.archived_at_unix_ms.is_some()
        {
            return Err("terminal reservation does not match bootstrap".into());
        }
        Ok(Self { db, cap })
    }
    pub fn startup_failed(&self, message: &str, may_have_started: bool) -> Result<(), String> {
        let mut snapshot = self.snapshot()?;
        snapshot.status = if may_have_started {
            crate::protocol::Status::OutcomeUnknown
        } else {
            crate::protocol::Status::Failed
        };
        snapshot.startup_error = Some(message.chars().take(256).collect());
        self.save(&snapshot)
    }
    pub fn snapshot(&self) -> Result<Snapshot, String> {
        read_snapshot(&self.db)
    }
    pub fn save(&self, snapshot: &Snapshot) -> Result<(), String> {
        self.db
            .execute(
                "UPDATE snapshot SET data=?1",
                [serde_json::to_string(snapshot).map_err(|_| "invalid terminal snapshot")?],
            )
            .map_err(|_| "cannot save terminal snapshot")?;
        Ok(())
    }
    pub fn append(&mut self, bytes: &[u8]) -> Result<(), String> {
        let mut s = self.snapshot()?;
        let start = s.end_offset;
        s.end_offset = s
            .end_offset
            .checked_add(bytes.len() as u64)
            .filter(|n| *n <= i64::MAX as u64)
            .ok_or("terminal cursor overflow")?;
        s.start_offset = s.end_offset.saturating_sub(self.cap as u64);
        let tx = self
            .db
            .transaction()
            .map_err(|_| "cannot begin spool transaction")?;
        tx.execute(
            "INSERT INTO chunks VALUES(?1,?2)",
            params![start as i64, bytes],
        )
        .map_err(|_| "cannot persist terminal output")?;
        // Restrict the range by primary key before reading any BLOB lengths.
        tx.execute(
            "DELETE FROM chunks WHERE offset<=?1 AND offset+length(data)<=?1",
            [s.start_offset as i64],
        )
        .map_err(|_| "cannot prune terminal output")?;
        // Trim the first overlapping chunk as well, bounding logical retention
        // exactly even when the cap is not a multiple of the reader chunk size.
        tx.execute(
            "UPDATE chunks SET data=substr(data,?1-offset+1),offset=?1 WHERE offset<?1",
            [s.start_offset as i64],
        )
        .map_err(|_| "cannot trim terminal output")?;
        tx.execute(
            "UPDATE snapshot SET data=?1",
            [serde_json::to_string(&s).map_err(|_| "invalid terminal snapshot")?],
        )
        .map_err(|_| "cannot save terminal cursor")?;
        tx.commit().map_err(|_| "cannot commit terminal output")?;
        Ok(())
    }
    pub fn output(&self, after: u64, limit: usize) -> Result<Output, String> {
        read_output(&self.db, OutputCursor::After(after), limit)
    }
    /// Reserve before an irreversible PTY write. A crash never authorizes a
    /// retry: a reserved ID without a result is explicitly uncertain.
    pub fn reserve(
        &self,
        id: &str,
        request: &str,
        shutdown: bool,
    ) -> Result<Option<MutationResult>, String> {
        use rusqlite::OptionalExtension;
        let old: Option<(String, Option<String>)> = self
            .db
            .query_row(
                "SELECT request,result FROM operations WHERE id=?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
            .map_err(|_| "cannot inspect operation")?;
        if let Some((old, result)) = old {
            if old != request {
                return Err("operation id reused with different request".into());
            }
            return result
                .map(|s| {
                    serde_json::from_str(&s).map_err(|_| String::from("invalid operation result"))
                })
                .transpose()?
                .map(Some)
                .ok_or_else(|| "operation outcome unknown; input must not be replayed".into());
        }
        let count: u32 = self
            .db
            .query_row("SELECT COUNT(*) FROM operations", [], |r| r.get(0))
            .map_err(|_| "cannot inspect operation budget")?;
        if count >= if shutdown { 1026 } else { 1024 } {
            return Err("terminal operation budget exhausted".into());
        }
        self.db
            .execute(
                "INSERT INTO operations(id,request) VALUES(?1,?2)",
                params![id, request],
            )
            .map_err(|_| "cannot reserve terminal operation")?;
        Ok(None)
    }
    pub fn finish(&self, id: &str, result: &MutationResult) -> Result<(), String> {
        self.db
            .execute(
                "UPDATE operations SET result=?2 WHERE id=?1",
                params![
                    id,
                    serde_json::to_string(result)
                        .map_err(|_| "invalid terminal operation result")?
                ],
            )
            .map_err(|_| "cannot save terminal operation result")?;
        Ok(())
    }
}
fn read_snapshot(db: &Connection) -> Result<Snapshot, String> {
    let text: String = db
        .query_row("SELECT data FROM snapshot", [], |r| r.get(0))
        .map_err(|_| "terminal snapshot unavailable")?;
    let snapshot: Snapshot =
        serde_json::from_str(&text).map_err(|_| "invalid terminal snapshot")?;
    if snapshot.start_offset > snapshot.end_offset || snapshot.end_offset > i64::MAX as u64 {
        return Err("invalid terminal spool offsets".into());
    }
    Ok(snapshot)
}
#[derive(Clone, Copy)]
pub(crate) enum OutputCursor {
    After(u64),
    Tail,
}

fn read_output(db: &Connection, cursor: OutputCursor, limit: usize) -> Result<Output, String> {
    if !(1..=crate::protocol::READ_LIMIT).contains(&limit)
        || matches!(cursor, OutputCursor::After(after) if after > i64::MAX as u64)
    {
        return Err("invalid terminal cursor / output limit".into());
    }
    let s = read_snapshot(db)?;
    let after = match cursor {
        OutputCursor::After(after) => after,
        OutputCursor::Tail => s.end_offset.saturating_sub(limit as u64),
    };
    if after > s.end_offset {
        return Err("terminal output cursor is in the future".into());
    }
    let from = after.max(s.start_offset);
    let count = (s.end_offset - from).min(limit as u64) as usize;
    let data = read_chunks(db, from, count)?;
    Ok(Output {
        next: from + data.len() as u64,
        from,
        gap: match cursor {
            OutputCursor::After(_) => after < s.start_offset,
            OutputCursor::Tail => from != 0,
        },
        data_base64: base64::engine::general_purpose::STANDARD.encode(data),
        snapshot: s,
    })
}

fn read_chunks(db: &Connection, from: u64, count: usize) -> Result<Vec<u8>, String> {
    let mut data = Vec::with_capacity(count);
    if count == 0 {
        return Ok(data);
    }
    // Seek to the chunk containing `from`, then visit only the requested page.
    let mut stmt = db
        .prepare(
            "SELECT offset,data FROM chunks
                  WHERE offset >= COALESCE((SELECT MAX(offset) FROM chunks WHERE offset<=?1),?1)
                  ORDER BY offset",
        )
        .map_err(|_| "cannot query terminal output")?;
    let rows = stmt
        .query_map([from as i64], |r| {
            Ok((r.get::<_, i64>(0)? as u64, r.get::<_, Vec<u8>>(1)?))
        })
        .map_err(|_| "cannot query terminal output")?;
    for row in rows {
        let (offset, bytes) = row.map_err(|_| "cannot read terminal output")?;
        let position = from + data.len() as u64;
        let skip = position
            .checked_sub(offset)
            .ok_or("terminal spool has a gap")?;
        // Only the first chunk can overlap the requested range.
        if !data.is_empty() && skip != 0 {
            return Err("terminal spool chunks overlap".into());
        }
        let skip = usize::try_from(skip).map_err(|_| "terminal spool has a gap")?;
        let part = bytes.get(skip..).ok_or("terminal spool has a gap")?;
        let n = part.len().min(count - data.len());
        data.extend_from_slice(&part[..n]);
        if data.len() == count {
            return Ok(data);
        }
    }
    Err("terminal spool output is incomplete".into())
}
/// Offline reads do not claim that an old PID is still alive. They preserve
/// the last durable bytes after a supervisor failure, without taking over it.
pub(crate) fn offline(dir: &Path, after: u64, limit: usize) -> Result<Output, String> {
    read_saved_output(dir, OutputCursor::After(after), limit)
}

pub(crate) fn read_saved_output(
    dir: &Path,
    cursor: OutputCursor,
    limit: usize,
) -> Result<Output, String> {
    let db = open_saved(dir)?;
    let tx = db
        .unchecked_transaction()
        .map_err(|_| "cannot snapshot terminal spool")?;
    let mut output = read_output(&tx, cursor, limit)?;
    mark_offline(&mut output.snapshot);
    Ok(output)
}

pub(crate) fn saved_snapshot(dir: &Path) -> Result<Snapshot, String> {
    let mut snapshot = read_snapshot(&open_saved(dir)?)?;
    mark_offline(&mut snapshot);
    Ok(snapshot)
}

fn open_saved(dir: &Path) -> Result<Connection, String> {
    let path = dir.join("output.db");
    crate::registry::regular_or_missing(&path)?;
    let db = Connection::open_with_flags(
        path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )
    .map_err(|_| "terminal spool unavailable")?;
    db.busy_timeout(READ_BUSY_TIMEOUT)
        .map_err(|_| "cannot bound terminal spool read")?;
    let version: u32 = db
        .pragma_query_value(None, "user_version", |r| r.get(0))
        .map_err(|error| match error {
            rusqlite::Error::SqliteFailure(code, _) => {
                format!(
                    "cannot read terminal spool version (SQLite {})",
                    code.extended_code
                )
            }
            _ => "invalid terminal spool version".into(),
        })?;
    if version != 1 {
        return Err("unsupported terminal spool version".into());
    }
    Ok(db)
}

fn mark_offline(snapshot: &mut Snapshot) {
    snapshot.supervisor_connected = false;
    if matches!(
        snapshot.status,
        crate::protocol::Status::Starting | crate::protocol::Status::Running
    ) {
        snapshot.status = crate::protocol::Status::OutcomeUnknown;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{READ_LIMIT, Status};
    struct Directory(std::path::PathBuf);
    impl Directory {
        fn new() -> Self {
            let p = std::env::temp_dir().join(format!("fliuno-spool-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir(&p).unwrap();
            Self(p)
        }
    }
    impl Drop for Directory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    fn snapshot() -> Snapshot {
        Snapshot {
            terminal_id: "a".repeat(64),
            session_id: "session".into(),
            preset_id: "preset".into(),
            status: Status::Running,
            startup_error: None,
            supervisor_connected: false,
            rows: 24,
            cols: 80,
            exit_code: None,
            signal: None,
            start_offset: 0,
            end_offset: 0,
            input_bytes: 0,
            output_closed: false,
            expires_at_unix_ms: 12345,
            archived_at_unix_ms: None,
        }
    }
    #[test]
    fn byte_log_preserves_binary_cursor_gaps_and_bounded_disk_retention() {
        let dir = Directory::new();
        let mut spool = Spool::create(&dir.0, 65536, &snapshot()).unwrap();
        let data: Vec<u8> = (0..1024 * 1024).map(|i| (i % 256) as u8).collect();
        for bytes in data.chunks(8192) {
            spool.append(bytes).unwrap();
        }
        let output = spool.output(0, READ_LIMIT).unwrap();
        assert!(output.gap);
        assert_eq!(output.from, (data.len() - 65536) as u64);
        let mut bytes = base64::engine::general_purpose::STANDARD
            .decode(output.data_base64)
            .unwrap();
        let second = spool.output(output.next, READ_LIMIT).unwrap();
        assert!(!second.gap);
        bytes.extend(
            base64::engine::general_purpose::STANDARD
                .decode(second.data_base64)
                .unwrap(),
        );
        assert_eq!(bytes, data[data.len() - 65536..]);
        assert_eq!(second.next, data.len() as u64);
        assert!(std::fs::metadata(dir.0.join("output.db")).unwrap().len() < 200 * 1024);
        assert!(spool.output(data.len() as u64 + 1, 1).is_err());
        assert!(spool.output(0, READ_LIMIT + 1).is_err());
        drop(spool);
        let recovered = offline(&dir.0, second.next, 1).unwrap();
        assert_eq!(recovered.snapshot.status, Status::OutcomeUnknown);
        assert_eq!(recovered.next, data.len() as u64);
    }
    #[test]
    fn tail_uses_one_durable_cursor_across_trimmed_chunks_and_later_appends() {
        let dir = Directory::new();
        let mut spool = Spool::create(&dir.0, 7, &snapshot()).unwrap();
        let empty = read_saved_output(&dir.0, OutputCursor::Tail, 5).unwrap();
        assert_eq!((empty.from, empty.next, empty.gap), (0, 0, false));
        spool.append(b"ab").unwrap();
        let full = read_saved_output(&dir.0, OutputCursor::Tail, 5).unwrap();
        assert_eq!((full.from, full.next, full.gap), (0, 2, false));
        spool.append(b"cdef").unwrap();
        spool.append(b"ghi").unwrap();
        let tail = read_saved_output(&dir.0, OutputCursor::Tail, 5).unwrap();
        assert_eq!((tail.from, tail.next, tail.gap), (4, 9, true));
        assert_eq!(tail.next, tail.snapshot.end_offset);
        let decode = |data| {
            base64::engine::general_purpose::STANDARD
                .decode(data)
                .unwrap()
        };
        assert_eq!(decode(tail.data_base64), b"efghi");
        let retained = read_saved_output(&dir.0, OutputCursor::Tail, 9).unwrap();
        assert_eq!((retained.from, retained.next, retained.gap), (2, 9, true));
        assert_eq!(decode(retained.data_base64), b"cdefghi");
        spool.append(b"jk").unwrap();
        let later = offline(&dir.0, tail.next, 5).unwrap();
        assert_eq!((later.from, later.next, later.gap), (9, 11, false));
        assert_eq!(decode(later.data_base64), b"jk");
    }

    #[test]
    fn output_bytes_and_snapshot_rollback_together() {
        let dir = Directory::new();
        let mut spool = Spool::create(&dir.0, 65536, &snapshot()).unwrap();
        spool.append(b"committed").unwrap();
        spool.db.execute_batch("CREATE TRIGGER fail_snapshot BEFORE UPDATE ON snapshot BEGIN SELECT RAISE(ABORT,'fixture'); END;").unwrap();
        assert!(spool.append(b"uncommitted").is_err());
        let o = spool.output(0, 100).unwrap();
        assert_eq!(o.next, 9);
        assert_eq!(
            base64::engine::general_purpose::STANDARD
                .decode(o.data_base64)
                .unwrap(),
            b"committed"
        );
    }
    #[test]
    fn fresh_reservation_is_readable_before_bootstrap_and_cannot_be_rebound() {
        let dir = Directory::new();
        let mut expected = snapshot();
        expected.status = Status::Starting;
        drop(Spool::create(&dir.0, 65536, &expected).unwrap());
        let pending = offline(&dir.0, 0, 1).unwrap();
        assert_eq!(pending.snapshot.status, Status::OutcomeUnknown);
        assert!(!pending.snapshot.supervisor_connected);
        assert_eq!(pending.data_base64, "");
        assert!(Spool::create(&dir.0, 65536, &expected).is_err());
        let mut other = expected.clone();
        other.session_id = "another-session".into();
        assert!(Spool::open_reserved(&dir.0, 65536, &other).is_err());
        let spool = Spool::open_reserved(&dir.0, 65536, &expected).unwrap();
        spool.startup_failed("host startup refused", false).unwrap();
        assert!(Spool::open_reserved(&dir.0, 65536, &expected).is_err());
        let failed = offline(&dir.0, 0, 1).unwrap();
        assert_eq!(failed.snapshot.status, Status::Failed);
        assert_eq!(
            failed.snapshot.startup_error.as_deref(),
            Some("host startup refused")
        );
    }
    #[test]
    fn saved_output_waits_for_a_slow_writer_commit() {
        let dir = Directory::new();
        let mut spool = Spool::create(&dir.0, 65536, &snapshot()).unwrap();
        spool.append(b"committed").unwrap();
        let tx = spool
            .db
            .transaction_with_behavior(rusqlite::TransactionBehavior::Exclusive)
            .unwrap();
        std::thread::scope(|scope| {
            let (started, in_flight) = std::sync::mpsc::channel();
            let dir = &dir.0;
            let reader = scope.spawn(move || {
                started.send(()).unwrap();
                offline(dir, 0, 100)
            });
            in_flight.recv().unwrap();
            std::thread::sleep(Duration::from_millis(300));
            tx.commit().unwrap();
            let output = reader.join().unwrap().unwrap();
            assert_eq!(output.next, 9);
            assert_eq!(
                base64::engine::general_purpose::STANDARD
                    .decode(output.data_base64)
                    .unwrap(),
                b"committed"
            );
        });
    }

    #[test]
    fn operation_reservation_and_completed_result_survive_reopen_without_replay() {
        let dir = Directory::new();
        let spool = Spool::create(&dir.0, 65536, &snapshot()).unwrap();
        assert!(spool.reserve("a", "request", false).unwrap().is_none());
        assert!(
            spool
                .reserve("a", "request", false)
                .unwrap_err()
                .contains("unknown")
        );
        assert!(
            spool
                .reserve("a", "different", false)
                .unwrap_err()
                .contains("different")
        );
        spool
            .finish("a", &Ok(serde_json::json!({"writtenBytes":3})))
            .unwrap();
        drop(spool);
        let reopened = Spool {
            db: Connection::open(dir.0.join("output.db")).unwrap(),
            cap: 65536,
        };
        assert_eq!(
            reopened
                .reserve("a", "request", false)
                .unwrap()
                .unwrap()
                .unwrap()["writtenBytes"],
            3
        );
    }
}
