use super::*;
use fliuno_protocol::TerminalArchiveBatchItem as Item;

#[test]
fn invalid_batch_is_rejected_in_full_before_any_output_or_identity_changes() {
    let fixture = Directory::new();
    let first = "a".repeat(64);
    let directory = fixture.spool(&first, "s", Status::Exited);
    for ids in [
        vec![],
        vec![first.clone(), "invalid-later-ID".into()],
        vec![first.clone(), first.clone()],
        (0..33).map(|number| format!("{number:064x}")).collect(),
    ] {
        assert!(fixture.service.archive_batch("s", &ids).is_err());
        assert!(directory.join("output.db").is_file());
        assert!(
            fixture
                .service
                .archives("s", 0, 100)
                .unwrap()
                .items
                .is_empty()
        );
    }
    let batch = fixture.service.archive_batch("s", &[first]).unwrap();
    assert!(matches!(batch.items[0], Item::Archived { .. }));
    assert!(!directory.exists());
}

#[test]
fn mixed_batch_preserves_order_live_owners_session_boundaries_and_retry_identity() {
    let fixture = Directory::new();
    let ids: Vec<_> = (0..5).map(|number| format!("{number:064x}")).collect();
    let stopped = fixture.spool(&ids[0], "s", Status::Exited);
    let live = fixture.spool(&ids[1], "s", Status::Running);
    let owner = owner_lock(&live).unwrap();
    let other = fixture.spool(&ids[2], "other", Status::Exited);
    // ids[3] is deliberately absent. ids[4] archives its identity but must
    // retain output while an unrecognized file blocks conservative cleanup.
    let pending = fixture.spool(&ids[4], "s", Status::Exited);
    fs::write(pending.join("unrecognized-file"), b"retain").unwrap();
    let batch = fixture.service.archive_batch("s", &ids).unwrap();
    assert_eq!(batch.items.len(), ids.len());
    let Item::Archived {
        terminal_id,
        archive: original,
    } = &batch.items[0]
    else {
        panic!("stopped terminal must archive")
    };
    assert_eq!(terminal_id, &ids[0]);
    assert!(!original.cleanup_pending);
    for (item, id) in batch.items[1..4].iter().zip(&ids[1..4]) {
        let Item::Failed { terminal_id, .. } = item else {
            panic!("non-archivable terminal must report its failure")
        };
        assert_eq!(terminal_id, id);
    }
    let Item::Archived {
        terminal_id,
        archive,
    } = &batch.items[4]
    else {
        panic!("pending cleanup is a committed archive")
    };
    assert_eq!(terminal_id, &ids[4]);
    assert!(archive.cleanup_pending);
    assert!(!stopped.exists());
    for directory in [&live, &other, &pending] {
        assert!(directory.join("output.db").is_file());
    }
    assert!(
        Registry::open(&fixture.service.root, false)
            .unwrap()
            .get(&ids[1])
            .unwrap()
            .is_none()
    );
    assert!(
        Registry::open(&fixture.service.root, false)
            .unwrap()
            .get(&ids[2])
            .unwrap()
            .is_none()
    );
    drop(owner);
    fs::remove_file(pending.join("unrecognized-file")).unwrap();
    let restarted = Service::new(
        fixture.service.root.as_ref().clone(),
        std::env::current_exe().unwrap(),
    )
    .unwrap();
    let retry = restarted.archive_batch("s", &ids).unwrap();
    let Item::Archived { archive, .. } = &retry.items[0] else {
        panic!("original tombstone missing")
    };
    assert_eq!(archive.snapshot, original.snapshot);
    let Item::Archived { archive, .. } = &retry.items[1] else {
        panic!("released owner should archive")
    };
    assert_eq!(archive.snapshot.status, Status::OutcomeUnknown);
    let Item::Archived { archive, .. } = &retry.items[4] else {
        panic!("pending tombstone missing")
    };
    assert!(!archive.cleanup_pending);
    assert!(!live.exists());
    assert!(other.exists());
    assert!(!pending.exists());
}

#[test]
fn per_item_storage_failure_does_not_undo_previous_archive_or_skip_later_ids() {
    let fixture = Directory::new();
    let ids: Vec<_> = (0..3).map(|number| format!("{number:064x}")).collect();
    let directories: Vec<_> = ids
        .iter()
        .map(|id| fixture.spool(id, "s", Status::Exited))
        .collect();
    let db = rusqlite::Connection::open(fixture.service.root.join("archive.db")).unwrap();
    db.execute_batch(&format!("CREATE TRIGGER refuse_one BEFORE INSERT ON archives WHEN NEW.id='{}' BEGIN SELECT RAISE(ABORT,'fixture storage failure'); END;",ids[1])).unwrap();
    let batch = fixture.service.archive_batch("s", &ids).unwrap();
    assert!(matches!(batch.items[0], Item::Archived { .. }));
    assert!(matches!(batch.items[1], Item::Failed { .. }));
    assert!(matches!(batch.items[2], Item::Archived { .. }));
    assert!(!directories[0].exists());
    assert!(directories[1].join("output.db").is_file());
    assert!(!directories[2].exists());
    db.execute_batch("DROP TRIGGER refuse_one").unwrap();
    drop(db);
    let retry = fixture.service.archive_batch("s", &ids).unwrap();
    assert!(
        retry
            .items
            .iter()
            .all(|item| matches!(item, Item::Archived { .. }))
    );
    for index in [0, 2] {
        let (
            Item::Archived {
                archive: before, ..
            },
            Item::Archived { archive: after, .. },
        ) = (&batch.items[index], &retry.items[index])
        else {
            unreachable!()
        };
        assert_eq!(before.snapshot, after.snapshot);
    }
    assert!(!directories[1].exists());
}

#[test]
fn blocked_commit_is_reported_as_unknown_with_evidence_retained_until_inspection() {
    let fixture = Directory::new();
    let id = "e".repeat(64);
    let directory = fixture.spool(&id, "s", Status::Exited);
    // DELETE-journal readers allow the insert but block the EXCLUSIVE lock at
    // COMMIT. This uses real SQLite locking, not a simulated error result.
    let reader = rusqlite::Connection::open(fixture.service.root.join("archive.db")).unwrap();
    reader
        .execute_batch("BEGIN; SELECT COUNT(*) FROM archives;")
        .unwrap();
    let batch = fixture
        .service
        .archive_batch("s", std::slice::from_ref(&id))
        .unwrap();
    assert!(
        matches!(&batch.items[0], Item::OutcomeUnknown { terminal_id, message } if terminal_id == &id && message.contains("commit outcome unknown"))
    );
    assert!(directory.join("output.db").is_file());
    assert!(directory.join("endpoint.json").is_file());
    reader.execute_batch("ROLLBACK;").unwrap();
    drop(reader);
    // Inspect before explicitly retrying. This fixture's rollback is known
    // locally, but the API conservatively preserves the general unknown case.
    assert!(
        fixture
            .service
            .archives("s", 0, 100)
            .unwrap()
            .items
            .is_empty()
    );
    let retry = fixture.service.archive_batch("s", &[id]).unwrap();
    assert!(matches!(retry.items[0], Item::Archived { .. }));
    assert!(!directory.exists());
}
