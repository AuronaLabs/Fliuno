//! Finite polling owned by one approved tool invocation. Each request is a
//! read; neither input nor remote cancellation is inferred from a snapshot.
use super::{McpService, ServiceError, fetch, validate_task_id};
use rmcp::model::{GetTaskResult, TaskStatus};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::time::Duration;
use tokio::time::{Instant, sleep_until};

const MAX_POLLS: u8 = 8;
const MAX_WAIT_MS: u64 = 5_000;
const DEFAULT_INTERVAL_MS: u64 = 500;
const MIN_INTERVAL_MS: u64 = 100;

pub(super) fn max_wait_ms(host_timeout: Duration) -> u64 {
    (host_timeout.as_millis() / 2).min(u128::from(MAX_WAIT_MS)) as u64
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct Args {
    task_id: String,
    wait_ms: u64,
}

impl Args {
    pub(super) fn parse(input: &Value, max_wait_ms: u64) -> Result<Self, String> {
        let args = Self::deserialize(input).map_err(|_| "invalid MCP task wait arguments")?;
        validate_task_id(&args.task_id)?;
        if !(1..=max_wait_ms).contains(&args.wait_ms) {
            return Err("MCP task waitMs exceeds its configured range".into());
        }
        Ok(args)
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Observation {
    reason: &'static str,
    polls: u8,
    snapshot: GetTaskResult,
}

pub(super) async fn observe(
    service: &McpService,
    args: Args,
) -> Result<Option<Observation>, ServiceError> {
    let deadline = Instant::now() + Duration::from_millis(args.wait_ms);
    for polls in 1..=MAX_POLLS {
        let Some(snapshot) = fetch(service, &args.task_id).await? else {
            return Ok(None);
        };
        let status = snapshot.task.status();
        let interval = snapshot
            .task
            .task
            .poll_interval_ms
            .unwrap_or(DEFAULT_INTERVAL_MS);
        let next = Instant::now().checked_add(Duration::from_millis(interval.max(MIN_INTERVAL_MS)));
        let reason = if status.is_terminal() {
            "terminal"
        } else if status == TaskStatus::InputRequired {
            "input_required"
        } else if polls == MAX_POLLS {
            "poll_limit"
        } else if let Some(next) = next.filter(|next| *next < deadline) {
            sleep_until(next).await;
            if Instant::now() < deadline {
                continue;
            }
            "wait_budget"
        } else {
            "wait_budget"
        };
        return Ok(Some(Observation {
            reason,
            polls,
            snapshot,
        }));
    }
    unreachable!("the last poll always returns")
}
