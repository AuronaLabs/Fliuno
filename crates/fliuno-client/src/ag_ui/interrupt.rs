//! The AG-UI approval interrupt and the bounded continuation input profile.
use super::run::{AgUiRunError, AgUiRunInput, request_key, validate_run_id};
use fliuno_protocol::{Approval, ApprovalDecision, ApprovalResumeParams};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgUiInterrupt {
    id: String,
    reason: &'static str,
    message: String,
    tool_call_id: String,
    response_schema: Value,
}

impl AgUiInterrupt {
    pub(super) fn approval(approval: Approval) -> Self {
        Self {
            id: approval.id,
            reason: "approval_required",
            message: format!("Review the request to run {}", approval.call.name),
            tool_call_id: approval.call.id,
            // Run clients receive the ID and schema; arguments are reserved for reviewers.
            response_schema: json!({
                "type":"object",
                "properties":{"decision":{"enum":["allow_once","deny"]}},
                "required":["decision"],
                "additionalProperties":false
            }),
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct ResumeEntry {
    interrupt_id: String,
    status: ResumeStatus,
    payload: Option<Decision>,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum ResumeStatus {
    Resolved,
    Cancelled,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Decision {
    decision: ApprovalDecision,
}

impl AgUiRunInput {
    pub(super) fn into_resume(
        self,
        session: &str,
    ) -> Result<(ApprovalResumeParams, String), AgUiRunError> {
        self.validate(session)?;
        let parent = self
            .parent_run_id
            .as_deref()
            .ok_or(AgUiRunError::UnsupportedInput)?;
        validate_run_id(parent)?;
        if parent == self.run_id || self.has_messages() {
            return Err(AgUiRunError::UnsupportedInput);
        }
        let [entry]: [ResumeEntry; 1] = self
            .resume
            .try_into()
            .map_err(|_| AgUiRunError::UnsupportedInput)?;
        if entry.interrupt_id.is_empty()
            || entry.interrupt_id.len() > 128
            || entry.interrupt_id.chars().any(char::is_control)
        {
            return Err(AgUiRunError::UnsupportedInput);
        }
        let decision = match (entry.status, entry.payload) {
            (ResumeStatus::Resolved, Some(payload)) => payload.decision,
            (ResumeStatus::Cancelled, None) => ApprovalDecision::Deny,
            _ => return Err(AgUiRunError::UnsupportedInput),
        };
        Ok((
            ApprovalResumeParams {
                session_id: session.to_owned(),
                parent_request_id: request_key(parent, true),
                client_request_id: request_key(&self.run_id, true),
                approval_id: entry.interrupt_id,
                decision,
            },
            self.run_id,
        ))
    }
}
