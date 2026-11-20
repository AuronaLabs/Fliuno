use super::*;

#[test]
fn historical_count_checkpoints_backfill_and_roll_back_with_task_completion() {
    let (mut kernel, session) = setup();
    let other = kernel.create_session(&session.workspace_id).unwrap();
    for _ in 0..127 {
        complete(&mut kernel, &session.id, "draft", "done");
    }
    let before = kernel.event_watermark().unwrap();
    complete(&mut kernel, &other.id, "unrelated", "done");
    complete(&mut kernel, &session.id, "first checkpoint", "done");
    let first = kernel.event_watermark().unwrap();
    complete(&mut kernel, &session.id, "after first checkpoint", "done");
    let next = kernel.event_watermark().unwrap();
    kernel
        .db
        .execute_batch("DROP TABLE session_turn_checkpoints; PRAGMA user_version=50;")
        .unwrap();
    let db = std::mem::replace(&mut kernel.db, Connection::open_in_memory().unwrap());
    drop(kernel);
    let mut restored = Kernel::from_connection(db, Box::new(DemoProvider), None).unwrap();
    for (through, expected) in [
        (0, 0),
        (before, 127),
        (first - 1, 127),
        (first, 128),
        (next, 129),
    ] {
        assert_eq!(
            restored.context_turn_count(&session.id, through).unwrap(),
            expected
        );
    }
    assert_eq!(restored.context_turn_count(&other.id, first).unwrap(), 1);
    let checkpoint: (i64, i64) = restored
        .db
        .query_row(
            "SELECT sequence,turn_count FROM session_turn_checkpoints WHERE session_id=?1",
            [&session.id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(checkpoint, (first as i64, 128));

    for _ in 129..255 {
        complete(&mut restored, &session.id, "draft", "done");
    }
    let task = restored
        .create_task_with_key(&session.id, "second checkpoint", None)
        .unwrap()
        .0;
    let PreparedModelStep::Ready { task, .. } = restored.prepare_model_step(&task.id).unwrap()
    else {
        panic!("not ready")
    };
    let before_failure = restored.event_watermark().unwrap();
    restored.db.execute_batch("CREATE TRIGGER reject_count_checkpoint BEFORE INSERT ON session_turn_checkpoints BEGIN SELECT RAISE(ABORT,'fixture'); END;").unwrap();
    let response = || Ok(ModelDecision::Complete("done".into()).into());
    assert!(
        restored
            .apply_model_response(task.clone(), response())
            .is_err()
    );
    assert_eq!(restored.event_watermark().unwrap(), before_failure);
    assert_eq!(
        restored
            .context_turn_count(&session.id, before_failure)
            .unwrap(),
        255
    );
    assert_eq!(
        restored.get_task(&task.id).unwrap().status,
        TaskStatus::Running
    );

    restored
        .db
        .execute_batch("DROP TRIGGER reject_count_checkpoint")
        .unwrap();
    restored.apply_model_response(task, response()).unwrap();
    let second = restored.event_watermark().unwrap();
    complete(&mut restored, &other.id, "unrelated", "done");
    complete(
        &mut restored,
        &session.id,
        "after second checkpoint",
        "done",
    );
    assert_eq!(
        restored
            .context_turn_count(&session.id, second - 1)
            .unwrap(),
        255
    );
    assert_eq!(
        restored.context_turn_count(&session.id, second).unwrap(),
        256
    );
    assert_eq!(
        restored
            .context_turn_count(&session.id, restored.event_watermark().unwrap())
            .unwrap(),
        257
    );
}
