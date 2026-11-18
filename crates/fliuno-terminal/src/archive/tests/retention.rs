use super::*;

const GRACE: u64 = 86_400_000;
const NOW: u64 = 10 * GRACE;

fn save(dir: &Path, expires: u64, modify: impl FnOnce(&mut Snapshot)) {
    let mut snapshot = spool::saved_snapshot(dir).unwrap();
    snapshot.expires_at_unix_ms = expires;
    modify(&mut snapshot);
    let db = rusqlite::Connection::open(dir.join("output.db")).unwrap();
    db.execute(
        "UPDATE snapshot SET data=?1",
        [serde_json::to_string(&snapshot).unwrap()],
    )
    .unwrap();
}

#[test]
fn retention_is_bounded_and_requires_age_settled_output_identity_and_unowned_record() {
    let fixture = Directory::new();
    for n in 0..9 {
        let id = format!("{n:064x}");
        save(
            &fixture.spool(&id, "s", Status::Exited),
            NOW - GRACE,
            |_| {},
        );
    }
    let held_id = "a".repeat(64);
    let held = fixture.spool(&held_id, "s", Status::Exited);
    save(&held, NOW - GRACE, |_| {});
    let owner = owner_lock(&held).unwrap();
    for (id, status, expiry, incomplete) in [
        ("b", Status::Exited, NOW - GRACE + 1, false),
        ("c", Status::OutcomeUnknown, NOW - GRACE, false),
        ("d", Status::Exited, NOW - GRACE, true),
    ] {
        let id = id.repeat(64);
        save(&fixture.spool(&id, "s", status), expiry, |snapshot| {
            snapshot.output_closed = !incomplete;
        });
    }
    let mismatched = "e".repeat(64);
    let dir = fixture.spool(&mismatched, "s", Status::Exited);
    save(&dir, NOW - GRACE, |snapshot| {
        snapshot.terminal_id = "f".repeat(64)
    });
    assert!(fixture.service.archive("s", &mismatched).is_err());

    let _registry_lock = registry::lock(&fixture.service.root).unwrap();
    let mut ledger = Registry::open(&fixture.service.root, true).unwrap();
    retention::sweep(&fixture.service, &mut ledger, NOW, GRACE).unwrap();
    assert_eq!(ledger.list("s", 0, 100).unwrap().items.len(), 8);
    retention::sweep(&fixture.service, &mut ledger, NOW, GRACE).unwrap();
    assert_eq!(ledger.list("s", 0, 100).unwrap().items.len(), 9);
    for id in ["a", "b", "c", "d", "e", "f"].map(|c| c.repeat(64)) {
        assert!(
            ledger.get(&id).unwrap().is_none(),
            "unexpected archive for {id}"
        );
    }
    assert!(dir.join("output.db").is_file());

    drop(owner);
    retention::sweep(&fixture.service, &mut ledger, NOW + 1, GRACE).unwrap();
    assert_eq!(ledger.list("s", 0, 100).unwrap().items.len(), 11);
    assert!(!held.exists());
}

#[test]
fn failed_retention_commit_preserves_spool_and_can_retry_after_reopen() {
    let fixture = Directory::new();
    let id = "a".repeat(64);
    let dir = fixture.spool(&id, "s", Status::Failed);
    save(&dir, NOW - GRACE, |snapshot| snapshot.output_closed = false);
    let _lock = registry::lock(&fixture.service.root).unwrap();
    let db = rusqlite::Connection::open(fixture.service.root.join("archive.db")).unwrap();
    db.execute_batch("CREATE TRIGGER fail_retention BEFORE INSERT ON archives BEGIN SELECT RAISE(ABORT,'fixture failure'); END;").unwrap();
    let mut ledger = Registry::open(&fixture.service.root, true).unwrap();
    assert!(retention::sweep(&fixture.service, &mut ledger, NOW, GRACE).is_err());
    assert!(ledger.get(&id).unwrap().is_none());
    assert!(dir.join("output.db").is_file());
    drop(ledger);

    db.execute_batch("DROP TRIGGER fail_retention").unwrap();
    let mut ledger = Registry::open(&fixture.service.root, true).unwrap();
    retention::sweep(&fixture.service, &mut ledger, NOW, GRACE).unwrap();
    let record = ledger.get(&id).unwrap().unwrap();
    assert_eq!(record.snapshot.status, Status::Failed);
    assert!(!record.snapshot.output_closed);
    assert!(!record.cleanup_pending);
    assert!(!dir.exists());
}

#[test]
fn capacity_pressure_reclaims_only_an_eligible_slot_before_a_new_reservation() {
    let fixture = Directory::new();
    let old = format!("{:064x}", 0);
    for index in 0..64 {
        let id = format!("{index:064x}");
        let dir = fixture.spool(&id, "s", Status::Exited);
        if id == old {
            save(&dir, now_ms() - GRACE, |_| {});
        }
    }
    let worker = fixture.root.join("invalid-worker.exe");
    fs::write(&worker, b"not a native executable").unwrap();
    let mut service = Service::new(fixture.service.root.as_ref().clone(), worker).unwrap();
    service.retention_grace_ms = Some(GRACE);
    let preset = Preset::resolve(
        crate::PresetConfig {
            id: "probe".into(),
            command: std::env::current_exe().unwrap(),
            isolation: Isolation::TrustedHost,
            args: vec![],
            env_allow: vec![],
            lifetime_ms: 30_000,
            spool_bytes: 65_536,
            resources: Default::default(),
            linux_cgroup_parent: None,
        },
        &service.root,
    )
    .unwrap();
    let workspace = fliuno_protocol::Workspace {
        id: "w".into(),
        root: fixture.root.to_string_lossy().into_owned(),
    };
    let context = ToolContext {
        task_id: "task",
        session_id: "s",
        workspace: &workspace,
    };
    let rt = crate::service::runtime().unwrap();
    let id = identity(&[&preset.identity, context.task_id, "create"]);
    assert!(
        rt.block_on(service.create(
            &preset,
            &context,
            "create",
            CreateOptions {
                args: vec![],
                rows: 24,
                cols: 80
            },
            &CancellationToken::new(),
        ))
        .is_err()
    );
    let page = service.archives("s", 0, 100).unwrap();
    assert_eq!(page.items.len(), 1);
    assert_eq!(page.items[0].snapshot.terminal_id, old);
    assert!(!page.items[0].cleanup_pending);
    assert_eq!(
        rt.block_on(service.status("s", &id)).unwrap().status,
        Status::Failed
    );
    assert_eq!(rt.block_on(service.list("s")).unwrap().len(), 64);
}
