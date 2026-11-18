//! Explicit selection never substitutes a different completed task or silently
//! drops a requested record to fit. It retains the existing snapshot contract.
use super::*;

fn options(ids: &[&str]) -> ContextOptions {
    ContextOptions {
        task_ids: Some(ids.iter().map(|id| (*id).into()).collect()),
        ..Default::default()
    }
}

#[test]
fn selected_history_skips_unrelated_work_and_normalizes_idempotency_without_changing_completion_order()
 {
    let (mut kernel, session) = setup();
    let first = complete(&mut kernel, &session.id, "relevant first", "first answer");
    let second = complete(&mut kernel, &session.id, "relevant second", "second answer");
    complete(
        &mut kernel,
        &session.id,
        "unrelated newest",
        &"x".repeat(262144),
    );
    assert!(
        kernel
            .session_context(&session.id, ContextOptions::default())
            .unwrap()
            .turns
            .is_empty()
    );
    let selected = options(&[&second.id, &first.id]);
    let preview = kernel
        .session_context(&session.id, selected.clone())
        .unwrap();
    assert_eq!(
        preview
            .turns
            .iter()
            .map(|turn| &turn.task_id)
            .collect::<Vec<_>>(),
        vec![&first.id, &second.id]
    );
    assert_eq!(preview.omitted_turns, 1);
    assert!(
        !context_prompt(Some(&preview))
            .unwrap()
            .unwrap()
            .contains("private-reasoning")
    );
    let (task, created) = kernel
        .create_task_with_context(
            &session.id,
            "use selected",
            Some("selected"),
            TaskBackend::Model,
            Some(selected),
        )
        .unwrap();
    assert!(created);
    let frozen = serde_json::to_value(task.context.as_ref().unwrap()).unwrap();
    assert_eq!(frozen, serde_json::to_value(preview).unwrap());
    complete(&mut kernel, &session.id, "later", "not in frozen input");
    let (retry, created) = kernel
        .create_task_with_context(
            &session.id,
            "use selected",
            Some("selected"),
            TaskBackend::Model,
            Some(options(&[&first.id, &second.id])),
        )
        .unwrap();
    assert!(!created);
    assert_eq!(serde_json::to_value(retry.context).unwrap(), frozen);
    assert!(matches!(
        kernel.create_task_with_context(
            &session.id,
            "use selected",
            Some("selected"),
            TaskBackend::Model,
            Some(options(&[&first.id]))
        ),
        Err(Error::Conflict(_))
    ));
}

#[test]
fn selection_rejects_invalid_foreign_and_unfinished_ids_without_persisting_a_task_or_event() {
    let (mut kernel, session) = setup();
    let done = complete(&mut kernel, &session.id, "known", "answer");
    let other = kernel.create_session(&session.workspace_id).unwrap();
    let foreign = complete(
        &mut kernel,
        &other.id,
        "private other session",
        "OTHER_SECRET",
    );
    let running = kernel
        .create_task_with_key(&session.id, "unfinished", None)
        .unwrap()
        .0;
    let count: (i64, i64) = kernel
        .db
        .query_row(
            "SELECT (SELECT COUNT(*) FROM tasks), (SELECT COUNT(*) FROM events)",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    let mut isolated = options(&[&done.id]);
    isolated.mode = ContextMode::Isolated;
    let invalid = [
        options(&[]),
        options(&[&done.id, &done.id]),
        options(&[""]),
        options(&["has space"]),
        options(&[&"a".repeat(129)]),
        options(&[&foreign.id]),
        options(&[&running.id]),
        options(&["missing"]),
        ContextOptions {
            max_turns: 1,
            ..options(&[&done.id, &running.id])
        },
        isolated,
    ];
    for option in invalid {
        let error = kernel
            .session_context(&session.id, option.clone())
            .unwrap_err();
        assert!(matches!(error, Error::Invalid(_)));
        assert!(!error.to_string().contains("OTHER_SECRET"));
        assert!(matches!(
            kernel.create_task_with_context(
                &session.id,
                "invalid selection",
                Some("unclaimed-key"),
                TaskBackend::Model,
                Some(option)
            ),
            Err(Error::Invalid(_))
        ));
    }
    let after = kernel
        .db
        .query_row(
            "SELECT (SELECT COUNT(*) FROM tasks), (SELECT COUNT(*) FROM events)",
            [],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
        )
        .unwrap();
    assert_eq!(after, count);
    assert!(
        kernel
            .create_task_with_context(
                &session.id,
                "valid selection",
                Some("unclaimed-key"),
                TaskBackend::Model,
                Some(options(&[&done.id]))
            )
            .unwrap()
            .1
    );
}

#[test]
fn explicit_selection_respects_raw_and_complete_snapshot_byte_budgets_without_partial_results() {
    let (mut kernel, session) = setup();
    let first = complete(&mut kernel, &session.id, "first", &"a".repeat(600));
    let second = complete(&mut kernel, &session.id, "second", &"b".repeat(600));
    let opts = options(&[&first.id, &second.id]);
    let snapshot = kernel.session_context(&session.id, opts.clone()).unwrap();
    let bytes = serde_json::to_vec(&snapshot).unwrap().len() as u32;
    assert!(bytes > 1024);
    let exact = kernel
        .session_context(
            &session.id,
            ContextOptions {
                max_bytes: bytes,
                ..opts.clone()
            },
        )
        .unwrap();
    // The budget's own numeric representation is part of the serialized size.
    let exact_bytes = serde_json::to_vec(&exact).unwrap().len() as u32;
    assert!(exact_bytes <= bytes);
    assert!(
        kernel
            .session_context(
                &session.id,
                ContextOptions {
                    max_bytes: exact_bytes - 1,
                    ..opts
                }
            )
            .is_err()
    );
    let huge = complete(&mut kernel, &session.id, "huge", &"x".repeat(2048));
    assert!(
        kernel
            .session_context(
                &session.id,
                ContextOptions {
                    max_bytes: 1024,
                    ..options(&[&huge.id])
                }
            )
            .is_err()
    );
    let chosen = kernel
        .session_context(&session.id, options(&[&first.id]))
        .unwrap();
    assert_eq!(chosen.turns.len(), 1);
    assert_eq!(chosen.omitted_turns, 2);
}

#[test]
fn version_eight_upgrade_preserves_snapshots_and_selected_history_survives_disk_restart() {
    let directory =
        std::env::temp_dir().join(format!("fliuno-context-selection-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&directory).unwrap();
    let database = directory.join("kernel.db");
    let mut kernel = Kernel::open(&database, Box::new(DemoProvider)).unwrap();
    let workspace = kernel.open_workspace(directory.to_str().unwrap()).unwrap();
    let session = kernel.create_session(&workspace.id).unwrap();
    let source = complete(&mut kernel, &session.id, "source", "SOURCE_ANSWER");
    let old = kernel
        .create_task_with_key(&session.id, "old frozen input", Some("old"))
        .unwrap()
        .0;
    let old_json = serde_json::to_value(&old).unwrap();
    assert!(old_json["context"]["options"].get("taskIds").is_none());
    kernel.db.pragma_update(None, "user_version", 8).unwrap();
    drop(kernel);
    let mut kernel = Kernel::open(&database, Box::new(DemoProvider)).unwrap();
    assert_eq!(
        kernel
            .db
            .pragma_query_value(None, "user_version", |row| row.get::<_, u32>(0))
            .unwrap(),
        9
    );
    let recovered = kernel.get_task(&old.id).unwrap();
    assert_eq!(
        serde_json::to_value(&recovered.context).unwrap(),
        old_json["context"]
    );
    let (selected, _) = kernel
        .create_task_with_context(
            &session.id,
            "selected frozen input",
            Some("new"),
            TaskBackend::Model,
            Some(options(&[&source.id])),
        )
        .unwrap();
    let frozen = serde_json::to_value(&selected.context).unwrap();
    assert_eq!(
        selected.context.as_ref().unwrap().turns[0].task_id,
        source.id
    );
    drop(kernel);
    let mut kernel = Kernel::open(&database, Box::new(DemoProvider)).unwrap();
    let (retry, created) = kernel
        .create_task_with_context(
            &session.id,
            "selected frozen input",
            Some("new"),
            TaskBackend::Model,
            Some(options(&[&source.id])),
        )
        .unwrap();
    assert!(!created);
    assert_eq!(serde_json::to_value(retry.context).unwrap(), frozen);
    drop(kernel);
    std::fs::remove_dir_all(directory).unwrap();
}
