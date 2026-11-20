//! Aggregate a bounded page directly from the committed usage event range.
use super::*;

impl Kernel {
    pub fn summarize_model_usage_range(
        &self,
        request: &ModelUsageRangeParams,
    ) -> Result<ModelUsageRangePage> {
        if !(1..=MAX_MODEL_USAGE_RANGE_REPORTS).contains(&request.limit) {
            return Err(Error::Invalid(format!(
                "limit must be between 1 and {MAX_MODEL_USAGE_RANGE_REPORTS}"
            )));
        }
        let scope = match &request.scope {
            ModelUsageSummaryScope::Task { task_id } => UsageScope::Task(task_id),
            ModelUsageSummaryScope::Session { session_id } => UsageScope::Session(session_id),
            ModelUsageSummaryScope::Workspace { workspace_id } => {
                UsageScope::Workspace(workspace_id)
            }
        };
        let tx = self.db.unchecked_transaction()?;
        let (owner, column) = scope.resolve(self)?;
        let watermark = self.event_watermark()?;
        let through_sequence = request.through.unwrap_or(watermark);
        if request.after > through_sequence || through_sequence > watermark {
            return Err(Error::Invalid(
                "model usage range requires after <= through <= current event sequence".into(),
            ));
        }
        let mut totals = ModelUsageTotals::default();
        let mut next_after = request.after;
        let has_more = {
            // Keep the scope index as the outer range. The extra row determines
            // whether another page is needed without deserializing its payload.
            let mut statement = tx.prepare_cached(&format!(
                "SELECT u.sequence,e.data
                 FROM model_usage u CROSS JOIN events e ON e.sequence=u.sequence
                 WHERE u.{column}=?1 AND u.sequence>?2 AND u.sequence<=?3
                 ORDER BY u.sequence LIMIT ?4"
            ))?;
            let mut rows = statement.query(params![
                owner,
                validate_event_cursor(request.after)?,
                validate_event_cursor(through_sequence)?,
                request.limit + 1
            ])?;
            for _ in 0..request.limit {
                let Some(row) = rows.next()? else { break };
                next_after = row.get::<_, i64>(0)? as u64;
                let EventKind::ModelUsageReported { usage, .. } =
                    serde_json::from_str(row.get_ref(1)?.as_str().map_err(rusqlite::Error::from)?)?
                else {
                    return Err(rusqlite::Error::InvalidQuery.into());
                };
                totals::accumulate(&mut totals, &usage);
            }
            rows.next()?.is_some()
        };
        tx.commit()?;
        Ok(ModelUsageRangePage {
            through_sequence,
            next_after: if has_more {
                next_after
            } else {
                through_sequence
            },
            has_more,
            totals,
        })
    }
}
