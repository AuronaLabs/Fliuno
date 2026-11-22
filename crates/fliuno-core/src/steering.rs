use super::*;
use rusqlite::{OptionalExtension, Transaction, params};

pub(crate) fn migrate(db: &Connection) -> Result<()> {
    db.execute_batch(
        "BEGIN IMMEDIATE;
        CREATE TABLE IF NOT EXISTS task_steering(
            task_id TEXT NOT NULL REFERENCES tasks(id),
            request_id TEXT NOT NULL,
            instruction TEXT NOT NULL,
            expected_sequence INTEGER NOT NULL,
            accepted_sequence INTEGER NOT NULL,
            attached_step INTEGER,
            PRIMARY KEY(task_id, request_id)
        );
        PRAGMA user_version=70; COMMIT;",
    )?;
    Ok(())
}

pub(crate) fn record(
    tx: &Transaction<'_>,
    task_id: &str,
    event: &EventKind,
    sequence: i64,
) -> Result<()> {
    match event {
        EventKind::TaskSteeringAccepted {
            client_request_id,
            expected_sequence,
            instruction,
        } => {
            tx.execute(
                "INSERT INTO task_steering(task_id, request_id, expected_sequence, instruction, accepted_sequence)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![task_id, client_request_id, *expected_sequence as i64, instruction, sequence],
            )?;
        }
        EventKind::TaskSteeringAttached {
            step,
            client_request_ids,
        } => {
            let mut attach = tx.prepare_cached(
                "UPDATE task_steering SET attached_step=?3 WHERE task_id=?1 AND request_id=?2 AND attached_step IS NULL"
            )?;
            for id in client_request_ids {
                if attach.execute(params![task_id, id, step])? != 1 {
                    return Err(Error::Conflict(
                        "steering request is no longer pending".into(),
                    ));
                }
            }
        }
        _ => {}
    }
    Ok(())
}

fn receipt(row: &rusqlite::Row<'_>) -> rusqlite::Result<TaskSteeringReceipt> {
    Ok(TaskSteeringReceipt {
        client_request_id: row.get(0)?,
        accepted_sequence: row.get::<_, i64>(1)? as u64,
        attached_step: row.get(2)?,
    })
}

impl Kernel {
    fn steering_watermark(&self, task_id: &str) -> Result<u64> {
        Ok(self
            .task_event_watermarks(&TaskWatermarksParams {
                task_ids: vec![task_id.into()],
            })?
            .through_sequences[0]
            .unwrap_or(0))
    }

    pub fn task_steering(&self, task_id: &str) -> Result<TaskSteeringState> {
        let status = self.get_task_summary(task_id)?.status;
        let through_sequence = self.steering_watermark(task_id)?;
        let mut select = self.db.prepare_cached(
            "SELECT request_id, accepted_sequence, attached_step FROM task_steering
             WHERE task_id=?1 ORDER BY accepted_sequence",
        )?;
        let receipts = select
            .query_map([task_id], receipt)?
            .collect::<rusqlite::Result<_>>()?;
        Ok(TaskSteeringState {
            status,
            through_sequence,
            receipts,
        })
    }

    pub fn steer_task(&mut self, p: &TaskSteerParams) -> Result<TaskSteeringReceipt> {
        crate::task::validate_key(&p.client_request_id)?;
        if p.instruction.trim().is_empty() || p.instruction.len() > MAX_TASK_STEERING_BYTES {
            return Err(Error::Invalid(
                "steering instruction must contain 1..4096 nonblank UTF-8 bytes".into(),
            ));
        }
        let task = self.get_task(&p.task_id)?;
        if let Some((saved, expected, instruction)) = self.db.query_row(
            "SELECT request_id, accepted_sequence, attached_step, expected_sequence, instruction
             FROM task_steering WHERE task_id=?1 AND request_id=?2",
            params![p.task_id, p.client_request_id],
            |row| Ok((receipt(row)?, row.get::<_, i64>(3)? as u64, row.get::<_, String>(4)?)),
        ).optional()? {
            return if expected == p.expected_sequence && instruction == p.instruction {
                Ok(saved)
            } else {
                Err(Error::Conflict("steering request ID was already used".into()))
            };
        }
        if task.backend != TaskBackend::Model
            || !matches!(
                task.status,
                TaskStatus::Running | TaskStatus::WaitingApproval | TaskStatus::WaitingInput
            )
        {
            return Err(Error::Conflict(
                "only active model tasks can accept steering".into(),
            ));
        }
        self.require_run_child_before_deadline(&task.id)?;
        self.require_run_child_authority(&task.id)?;
        if self.steering_watermark(&task.id)? != p.expected_sequence {
            return Err(Error::Conflict(
                "task changed; refresh its event watermark before steering".into(),
            ));
        }
        let count: u32 = self.db.query_row(
            "SELECT count(*) FROM task_steering WHERE task_id=?1",
            [&task.id],
            |row| row.get(0),
        )?;
        if count >= MAX_TASK_STEERING_REQUESTS {
            return Err(Error::Conflict(
                "task steering request limit reached".into(),
            ));
        }
        self.commit(
            &task,
            &[EventKind::TaskSteeringAccepted {
                client_request_id: p.client_request_id.clone(),
                expected_sequence: p.expected_sequence,
                instruction: p.instruction.clone(),
            }],
            &[],
        )?;
        self.db
            .query_row(
                "SELECT request_id, accepted_sequence, attached_step FROM task_steering
             WHERE task_id=?1 AND request_id=?2",
                params![task.id, p.client_request_id],
                receipt,
            )
            .map_err(Into::into)
    }

    pub(crate) fn has_pending_steering(&self, task_id: &str) -> Result<bool> {
        self.db.prepare_cached(
            "SELECT EXISTS(SELECT 1 FROM task_steering WHERE task_id=?1 AND attached_step IS NULL)"
        )?.query_row([task_id], |row| row.get(0)).map_err(Into::into)
    }

    pub(crate) fn attach_task_steering(&self, task: &mut Task) -> Result<Option<EventKind>> {
        let mut select = self.db.prepare_cached(
            "SELECT request_id, instruction FROM task_steering
             WHERE task_id=?1 AND attached_step IS NULL ORDER BY accepted_sequence",
        )?;
        let mut requests = select.query([&task.id])?;
        let mut ids = Vec::new();
        while let Some(row) = requests.next()? {
            ids.push(row.get(0)?);
            task.messages.push(Message::User { text: row.get(1)? });
        }
        Ok(
            (!ids.is_empty()).then_some(EventKind::TaskSteeringAttached {
                step: task.model_steps,
                client_request_ids: ids,
            }),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn instruction_arriving_during_model_step_is_attached_to_the_next_attempt() {
        let mut kernel = Kernel::in_memory(Box::new(DemoProvider)).unwrap();
        let workspace = kernel
            .open_workspace(std::env::current_dir().unwrap().to_str().unwrap())
            .unwrap();
        let session = kernel.create_session(&workspace.id).unwrap();
        let (task, _) = kernel
            .create_task_with_key(&session.id, "draft a note", None)
            .unwrap();
        let PreparedModelStep::Ready { task, .. } = kernel.prepare_model_step(&task.id).unwrap()
        else {
            panic!("expected first model step")
        };
        let before = kernel.task_steering(&task.id).unwrap();
        let request = TaskSteerParams {
            task_id: task.id.clone(),
            client_request_id: "revision-request".into(),
            expected_sequence: before.through_sequence,
            instruction: "make it shorter".into(),
        };
        let accepted = kernel.steer_task(&request).unwrap();
        assert!(accepted.attached_step.is_none());
        let stale = TaskSteerParams {
            client_request_id: "stale".into(),
            ..request.clone()
        };
        assert!(matches!(kernel.steer_task(&stale), Err(Error::Conflict(_))));
        let task = kernel
            .apply_model_response(task, Ok(ModelDecision::Complete("draft".into()).into()))
            .unwrap();
        assert_eq!(task.status, TaskStatus::Running);
        let PreparedModelStep::Ready { task, .. } = kernel.prepare_model_step(&task.id).unwrap()
        else {
            panic!("expected follow-up step")
        };
        assert!(
            matches!(task.messages.last(), Some(Message::User { text }) if text == "make it shorter")
        );
        let task = kernel
            .apply_model_response(task, Ok(ModelDecision::Complete("revised".into()).into()))
            .unwrap();
        assert_eq!(task.status, TaskStatus::Completed);
        assert_eq!(task.output.as_deref(), Some("revised"));
        let repeated = kernel.steer_task(&request).unwrap();
        assert_eq!(repeated.accepted_sequence, accepted.accepted_sequence);
        assert_eq!(repeated.attached_step, Some(2));
    }
}
