use super::*;
use crate::{
    config::{Isolation, Preset},
    service::{CreateOptions, identity},
    spool::Spool,
};
use fliuno_core::{CancellationToken, ToolContext};

#[path = "tests/batch.rs"]
mod batch;
#[path = "tests/retention.rs"]
mod retention_tests;

struct Directory {
    root: std::path::PathBuf,
    service: Service,
}
impl Directory {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("fliuno-archive-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&root).unwrap();
        let service =
            Service::new(root.join("terminals"), std::env::current_exe().unwrap()).unwrap();
        Self { root, service }
    }
    fn spool(&self, id: &str, session: &str, status: Status) -> std::path::PathBuf {
        let dir = self.service.root.join(id);
        fs::create_dir(&dir).unwrap();
        let snapshot = Snapshot {
            terminal_id: id.into(),
            session_id: session.into(),
            preset_id: "probe".into(),
            status,
            startup_error: None,
            supervisor_connected: false,
            rows: 24,
            cols: 80,
            exit_code: Some(0),
            signal: None,
            start_offset: 0,
            end_offset: 0,
            input_bytes: 0,
            output_closed: true,
            expires_at_unix_ms: now_ms(),
            archived_at_unix_ms: None,
        };
        let mut spool = Spool::create(&dir, 65536, &snapshot).unwrap();
        spool.append(b"private output\0\xff").unwrap();
        fs::write(dir.join("endpoint.json"), b"private capability").unwrap();
        dir
    }
}
impl Drop for Directory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[tokio::test]
async fn terminal_metadata_can_be_read_independently_of_output_chunks() {
    let fixture = Directory::new();
    let id = "a".repeat(64);
    let dir = fixture.spool(&id, "s", Status::Exited);
    fixture.spool(&"b".repeat(64), "other", Status::Exited);
    let db = rusqlite::Connection::open(dir.join("output.db")).unwrap();
    db.execute_batch("DROP TABLE chunks").unwrap();
    drop(db);
    let listed = fixture.service.list("s").await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].terminal_id, id);
    assert_eq!(listed[0].status, Status::Exited);
    assert_eq!(fixture.service.status("s", &id).await.unwrap(), listed[0]);
    assert!(fixture.service.status("other", &id).await.is_err());
    assert!(fixture.service.output("s", &id, 0, 8).await.is_err());
}

#[tokio::test]
async fn archive_reclaims_spool_but_preserves_session_identity_offsets_and_explicit_gaps() {
    let fixture = Directory::new();
    let service = &fixture.service;
    let id = "a".repeat(64);
    let dir = fixture.spool(&id, "s", Status::Exited);
    assert!(service.archive("other", &id).is_err());
    assert!(
        Registry::open(&service.root, false)
            .unwrap()
            .get(&id)
            .unwrap()
            .is_none()
    );
    let archive = service.archive("s", &id).unwrap();
    assert!(!archive.cleanup_pending);
    assert!(!dir.exists());
    assert!(archive.snapshot.archived_at_unix_ms.is_some());
    assert_eq!(archive.snapshot.start_offset, archive.snapshot.end_offset);
    assert!(archive.snapshot.end_offset > 0);
    assert_eq!(service.status("s", &id).await.unwrap(), archive.snapshot);
    let out = service.output("s", &id, 0, 8).await.unwrap();
    assert!(out.gap);
    assert!(out.data_base64.is_empty());
    assert_eq!(out.from, archive.snapshot.end_offset);
    assert_eq!(out.next, archive.snapshot.end_offset);
    assert!(!service.output("s", &id, out.next, 8).await.unwrap().gap);
    assert!(service.output("s", &id, out.next + 1, 8).await.is_err());
    assert!(service.output("other", &id, 0, 8).await.is_err());
    assert!(
        service
            .call(
                "s",
                &id,
                Operation::Kill {
                    operation_id: "b".repeat(64)
                }
            )
            .await
            .is_err()
    );
    assert_eq!(
        service.archive("s", &id).unwrap().snapshot,
        archive.snapshot
    );
    assert!(service.list("s").await.unwrap().is_empty());
    let mut subscription = service.subscribe("s", &id, 0, 8).await.unwrap();
    assert!(subscription.next().await.unwrap().unwrap().gap);
    assert!(subscription.next().await.unwrap().is_none());
    assert_eq!(
        subscription.end_reason(),
        Some(fliuno_protocol::TerminalSubscriptionEndReason::Archived)
    );
}

#[test]
fn archive_refuses_a_live_owner_and_preserves_uncertain_outcome_after_owner_exit() {
    let fixture = Directory::new();
    let id = "c".repeat(64);
    let dir = fixture.spool(&id, "s", Status::Running);
    let owner = owner_lock(&dir).unwrap();
    assert!(fixture.service.archive("s", &id).is_err());
    assert!(
        Registry::open(&fixture.service.root, false)
            .unwrap()
            .get(&id)
            .unwrap()
            .is_none()
    );
    assert!(dir.join("output.db").is_file());
    drop(owner);
    let record = fixture.service.archive("s", &id).unwrap();
    assert_eq!(record.snapshot.status, Status::OutcomeUnknown);
}

#[test]
fn committed_archive_survives_interrupted_cleanup_and_restart_does_not_delete_unknown_files() {
    let fixture = Directory::new();
    let id = "d".repeat(64);
    let dir = fixture.spool(&id, "s", Status::Exited);
    let unexpected = dir.join("host-file");
    fs::write(&unexpected, b"keep me").unwrap();
    let record = fixture.service.archive("s", &id).unwrap();
    assert!(record.cleanup_pending);
    assert!(dir.join("output.db").is_file());
    let restarted = Service::new(
        fixture.service.root.as_ref().clone(),
        std::env::current_exe().unwrap(),
    )
    .unwrap();
    assert_eq!(fs::read(&unexpected).unwrap(), b"keep me");
    assert_eq!(
        restarted.archives("s", 0, 1).unwrap().items[0].snapshot,
        record.snapshot
    );
    assert!(restarted.archives("s", 0, 1).unwrap().items[0].cleanup_pending);
    fs::remove_file(unexpected).unwrap();
    let _restarted = Service::new(
        fixture.service.root.as_ref().clone(),
        std::env::current_exe().unwrap(),
    )
    .unwrap();
    assert!(!dir.exists());
    let record = restarted.archives("s", 0, 1).unwrap().items.remove(0);
    assert!(!record.cleanup_pending);
    assert!(record.snapshot.archived_at_unix_ms.is_some());
}

#[test]
fn archive_pages_are_bounded_ordered_session_scoped_and_cleanup_retries_are_fair() {
    let fixture = Directory::new();
    for number in 0..10 {
        let id = format!("{number:064x}");
        let dir = fixture.spool(&id, if number == 9 { "other" } else { "s" }, Status::Exited);
        if number < 9 {
            fs::write(dir.join("unknown"), b"retain").unwrap();
        }
        fixture
            .service
            .archive(if number == 9 { "other" } else { "s" }, &id)
            .unwrap();
    }
    let first = fixture.service.archives("s", 0, 4).unwrap();
    assert_eq!(first.items.len(), 4);
    assert!(first.has_more);
    let second = fixture
        .service
        .archives("s", first.next_after, 100)
        .unwrap();
    assert_eq!(second.items.len(), 5);
    assert!(!second.has_more);
    assert!(
        fixture
            .service
            .archives("none", 0, 100)
            .unwrap()
            .items
            .is_empty()
    );
    assert!(fixture.service.archives("s", 0, 0).is_err());
    assert!(fixture.service.archives("s", u64::MAX, 100).is_err());
    let ledger = Registry::open(&fixture.service.root, false).unwrap();
    assert_eq!(ledger.pending(8).unwrap()[0], "0".repeat(64));
    let _lock = registry::lock(&fixture.service.root).unwrap();
    let ledger = Registry::open(&fixture.service.root, true).unwrap();
    recover(&fixture.service.root, &ledger).unwrap();
    assert_eq!(ledger.pending(8).unwrap()[0], format!("{:064x}", 8));
}

#[test]
fn removed_spools_free_capacity_but_archived_creation_identity_is_never_relaunched() {
    let fixture = Directory::new();
    let configuration = crate::PresetConfig {
        id: "probe".into(),
        command: std::env::current_exe().unwrap(),
        isolation: Isolation::TrustedHost,
        args: vec![],
        env_allow: vec![],
        lifetime_ms: 30000,
        spool_bytes: 65536,
        resources: Default::default(),
        linux_cgroup_parent: None,
    };
    let preset = Preset::resolve(configuration, &fixture.service.root).unwrap();
    let workspace = fliuno_protocol::Workspace {
        id: "w".into(),
        root: fixture.root.to_string_lossy().into_owned(),
    };
    let context = ToolContext {
        task_id: "task",
        session_id: "s",
        workspace: &workspace,
    };
    let original = identity(&[&preset.identity, "task", "create"]);
    fixture.spool(&original, "s", Status::Exited);
    for number in 0..63 {
        fixture.spool(&format!("{number:064x}"), "s", Status::Exited);
    }
    let options = || CreateOptions {
        args: vec![],
        rows: 24,
        cols: 80,
    };
    let runtime = crate::service::runtime().unwrap();
    let error = runtime
        .block_on(fixture.service.create(
            &preset,
            &context,
            "new",
            options(),
            &CancellationToken::new(),
        ))
        .unwrap_err();
    assert!(error.to_string().contains("capacity reached"));
    fixture.service.archive("s", &original).unwrap();
    let error = runtime
        .block_on(fixture.service.create(
            &preset,
            &context,
            "create",
            options(),
            &CancellationToken::new(),
        ))
        .unwrap_err();
    assert!(matches!(error, ToolError::OutcomeUnknownWithOutput { .. }));
    assert!(error.to_string().contains("archived"));
    assert!(!fixture.service.root.join(&original).exists());
    assert_eq!(
        runtime.block_on(fixture.service.list("s")).unwrap().len(),
        63
    );
    let worker = fixture.root.join("invalid-worker.exe");
    fs::write(&worker, b"not a native executable").unwrap();
    let replacement = Service::new(fixture.service.root.as_ref().clone(), worker).unwrap();
    let error = runtime
        .block_on(replacement.create(
            &preset,
            &context,
            "new",
            options(),
            &CancellationToken::new(),
        ))
        .unwrap_err();
    assert!(matches!(error, ToolError::Failed(_)));
    assert_eq!(
        runtime.block_on(fixture.service.list("s")).unwrap().len(),
        64
    );
}

#[test]
fn archive_insert_failure_does_not_remove_evidence_and_missing_ledger_cannot_reset_identity_history()
 {
    let fixture = Directory::new();
    let id = "e".repeat(64);
    let dir = fixture.spool(&id, "s", Status::Exited);
    let ledger_path = fixture.service.root.join("archive.db");
    {
        let db = rusqlite::Connection::open(&ledger_path).unwrap();
        db.execute_batch("CREATE TRIGGER refuse_archive BEFORE INSERT ON archives BEGIN SELECT RAISE(ABORT,'fixture storage failure'); END;").unwrap();
    }
    assert!(matches!(
        fixture.service.archive("s", &id).unwrap_err(),
        ToolError::Failed(_)
    ));
    assert!(dir.join("output.db").is_file());
    assert!(dir.join("endpoint.json").is_file());
    assert!(
        fixture
            .service
            .archives("s", 0, 100)
            .unwrap()
            .items
            .is_empty()
    );
    {
        let db = rusqlite::Connection::open(&ledger_path).unwrap();
        db.execute_batch("DROP TRIGGER refuse_archive").unwrap();
    }
    assert!(!fixture.service.archive("s", &id).unwrap().cleanup_pending);
    let backup = fixture.root.join("saved-ledger.db");
    fs::rename(&ledger_path, &backup).unwrap();
    assert!(
        Service::new(
            fixture.service.root.as_ref().clone(),
            std::env::current_exe().unwrap()
        )
        .is_err()
    );
    assert!(!ledger_path.exists());
    fs::write(&ledger_path, b"").unwrap();
    assert!(
        Service::new(
            fixture.service.root.as_ref().clone(),
            std::env::current_exe().unwrap()
        )
        .is_err()
    );
    assert!(backup.is_file());
}
