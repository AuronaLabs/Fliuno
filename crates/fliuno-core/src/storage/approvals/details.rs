//! Extract live approval details from one read view.
use super::*;
use rusqlite::Statement;
use std::collections::HashMap;

const PENDING_APPROVAL_DETAILS: &str = "SELECT p.kind, CASE p.kind
    WHEN 0 THEN json_extract(t.data,'$.pendingApproval')
    ELSE (SELECT a.value FROM json_each(t.data,'$.agentRun.pendingApprovals') a
          WHERE json_extract(a.value,'$.id')=p.approval_id)
    END
    FROM pending_approvals p CROSS JOIN tasks t ON t.id=p.task_id
    WHERE p.approval_id=?1 AND p.task_id=?2
      AND (p.expires_at IS NULL OR p.expires_at>?3)";

fn validate_approval_details_batch(request: &ApprovalGetBatchParams) -> Result<()> {
    if !(1..=MAX_APPROVAL_DETAILS_BATCH).contains(&request.approvals.len()) {
        return Err(Error::Invalid(format!(
            "approvals must contain 1 to {MAX_APPROVAL_DETAILS_BATCH} keys"
        )));
    }
    if request.approvals.iter().any(|key| {
        [&key.task_id, &key.approval_id].into_iter().any(|id| {
            !(1..=MAX_APPROVAL_DECISION_ID_BYTES).contains(&id.len())
                || !id.bytes().all(|byte| byte.is_ascii_graphic())
        })
    }) {
        return Err(Error::Invalid("invalid approval detail key".into()));
    }
    Ok(())
}

fn charge_approval_details(bytes: &mut usize, has_previous: bool, item_bytes: usize) -> Result<()> {
    *bytes += usize::from(has_previous) + item_bytes;
    if *bytes > MAX_APPROVAL_DETAILS_BATCH_BYTES {
        return Err(Error::Invalid(format!(
            "approval details exceed the {MAX_APPROVAL_DETAILS_BATCH_BYTES}-byte batch limit"
        )));
    }
    Ok(())
}

fn read_pending_approval(
    statement: &mut Statement<'_>,
    task_id: &str,
    approval_id: &str,
    now: i64,
) -> Result<Option<ApprovalDetails>> {
    let mut rows = statement.query(params![approval_id, task_id, now])?;
    let Some(row) = rows.next()? else {
        return Ok(None);
    };
    let kind = row.get(0)?;
    let data = row.get_ref(1)?.as_str().map_err(rusqlite::Error::from)?;
    Ok(Some(match decode_kind(kind)? {
        ApprovalKind::ToolCall => ApprovalDetails::ToolCall {
            approval: serde_json::from_str(data)?,
        },
        ApprovalKind::AgentPermission => ApprovalDetails::AgentPermission {
            approval: serde_json::from_str(data)?,
        },
        ApprovalKind::HostTool => ApprovalDetails::HostTool {
            approval: serde_json::from_str(data)?,
        },
    }))
}

impl Kernel {
    pub fn get_pending_approval(
        &self,
        task_id: &str,
        approval_id: &str,
    ) -> Result<ApprovalDetails> {
        let mut select = self.db.prepare_cached(PENDING_APPROVAL_DETAILS)?;
        let details = read_pending_approval(
            &mut select,
            task_id,
            approval_id,
            sql_timestamp(agent::now_ms()?)?,
        )?;
        details.ok_or_else(|| Error::NotFound("pending approval".into()))
    }

    pub fn get_pending_approvals(
        &self,
        request: &ApprovalGetBatchParams,
    ) -> Result<ApprovalGetBatch> {
        validate_approval_details_batch(request)?;
        let tx = self.db.unchecked_transaction()?;
        let now = sql_timestamp(agent::now_ms()?)?;
        let mut items: Vec<Option<ApprovalDetails>> = Vec::with_capacity(request.approvals.len());
        let mut seen: HashMap<&ApprovalDecisionKey, (usize, usize)> =
            HashMap::with_capacity(request.approvals.len());
        let mut bytes = 2; // Array brackets.
        {
            let mut select = tx.prepare_cached(PENDING_APPROVAL_DETAILS)?;
            for key in &request.approvals {
                let item = if let Some(&(index, item_bytes)) = seen.get(key) {
                    charge_approval_details(&mut bytes, !items.is_empty(), item_bytes)?;
                    items[index].clone()
                } else {
                    let item =
                        read_pending_approval(&mut select, &key.task_id, &key.approval_id, now)?;
                    let item_bytes = crate::json_size(&item)?;
                    charge_approval_details(&mut bytes, !items.is_empty(), item_bytes)?;
                    seen.insert(key, (items.len(), item_bytes));
                    item
                };
                items.push(item);
            }
        }
        tx.commit()?;
        Ok(ApprovalGetBatch { items })
    }
}
