//! Index committed approval decisions without duplicating event data.
use super::*;
use rusqlite::{Statement, Transaction};
use std::collections::HashMap;

const READ_DECISION: &str = "SELECT events.sequence,events.task_id,events.data
    FROM approval_decisions JOIN events USING(sequence)
    WHERE approval_decisions.task_id=?1 AND approval_id=?2";

pub(super) fn migrate(db: &mut Connection) -> Result<()> {
    let tx = db.transaction()?;
    tx.execute_batch(
        "CREATE TABLE IF NOT EXISTS approval_decisions(
            task_id TEXT NOT NULL REFERENCES tasks(id),
            approval_id TEXT NOT NULL,
            sequence INTEGER NOT NULL REFERENCES events(sequence),
            PRIMARY KEY(task_id,approval_id)
        ) WITHOUT ROWID;
        INSERT INTO approval_decisions(task_id,approval_id,sequence)
        SELECT task_id,json_extract(data,'$.approvalId'),sequence FROM events
        WHERE json_extract(data,'$.type') IN ('approval_resolved','agent_approval_resolved')
        ON CONFLICT(task_id,approval_id) DO NOTHING;
        PRAGMA user_version=34;",
    )?;
    tx.commit()?;
    Ok(())
}

pub(super) fn record(
    tx: &Transaction<'_>,
    task_id: &str,
    event: &EventKind,
    sequence: i64,
) -> Result<()> {
    if let EventKind::ApprovalResolved { approval_id, .. }
    | EventKind::AgentApprovalResolved { approval_id, .. } = event
    {
        tx.prepare_cached(
            "INSERT INTO approval_decisions(task_id,approval_id,sequence) VALUES(?1,?2,?3)",
        )?
        .execute(params![task_id, approval_id, sequence])?;
    }
    Ok(())
}

impl Kernel {
    pub fn get_approval_decision(&self, task_id: &str, approval_id: &str) -> Result<Event> {
        if approval_id.is_empty() || approval_id.len() > 128 {
            return Err(Error::Invalid("invalid approval ID".into()));
        }
        let mut statement = self.db.prepare_cached(READ_DECISION)?;
        read_decision(&mut statement, task_id, approval_id)?
            .ok_or_else(|| Error::NotFound("approval decision".into()))
    }

    pub fn get_approval_decisions(
        &self,
        request: &ApprovalDecisionBatchParams,
    ) -> Result<ApprovalDecisionBatch> {
        if !(1..=MAX_APPROVAL_DECISION_BATCH).contains(&request.approvals.len()) {
            return Err(Error::Invalid(format!(
                "approvals must contain 1 to {MAX_APPROVAL_DECISION_BATCH} keys"
            )));
        }
        if request.approvals.iter().any(|key| {
            [&key.task_id, &key.approval_id].into_iter().any(|id| {
                !(1..=MAX_APPROVAL_DECISION_ID_BYTES).contains(&id.len())
                    || !id.bytes().all(|byte| byte.is_ascii_graphic())
            })
        }) {
            return Err(Error::Invalid("invalid approval decision key".into()));
        }

        let tx = self.db.unchecked_transaction()?;
        let through_sequence = self.event_watermark()?;
        let mut items: Vec<Option<ApprovalDecisionReceipt>> =
            Vec::with_capacity(request.approvals.len());
        {
            let mut by_key: HashMap<&ApprovalDecisionKey, usize> =
                HashMap::with_capacity(request.approvals.len());
            let mut statement = tx.prepare_cached(READ_DECISION)?;
            for key in &request.approvals {
                // Store positions so duplicates also reuse a missing lookup.
                let item = match by_key.get(key) {
                    Some(&index) => items[index].clone(),
                    None => {
                        let item = read_decision(&mut statement, &key.task_id, &key.approval_id)?
                            .map(|event| receipt(key, event))
                            .transpose()?;
                        by_key.insert(key, items.len());
                        item
                    }
                };
                items.push(item);
            }
        }
        tx.commit()?;
        Ok(ApprovalDecisionBatch {
            items,
            through_sequence,
        })
    }
}

fn read_decision(
    statement: &mut Statement<'_>,
    task: &str,
    approval: &str,
) -> Result<Option<Event>> {
    let mut rows = statement.query(params![task, approval])?;
    rows.next()?.map(replay::event_row).transpose()
}

fn receipt(key: &ApprovalDecisionKey, event: Event) -> Result<ApprovalDecisionReceipt> {
    let invalid = || Error::Conflict("invalid persisted approval decision".into());
    let (approval_id, decision, run_id) = match event.data {
        EventKind::ApprovalResolved {
            approval_id,
            decision,
        } => (approval_id, decision, None),
        EventKind::AgentApprovalResolved {
            approval_id,
            decision,
            run_id,
        } => (approval_id, decision, Some(run_id)),
        _ => return Err(invalid()),
    };
    if event.task_id != key.task_id || approval_id != key.approval_id {
        return Err(invalid());
    }
    Ok(ApprovalDecisionReceipt {
        task_id: event.task_id,
        approval_id,
        decision,
        sequence: event.sequence,
        run_id,
    })
}
