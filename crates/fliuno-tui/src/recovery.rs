use crate::editor::Editor;
use crossterm::event::KeyCode;
use fliuno_protocol::{AgentTaskState, TaskReconcileParams, TaskResumeDisposition};

pub fn disposition_text(disposition: TaskResumeDisposition) -> &'static str {
    match disposition {
        TaskResumeDisposition::Ready => "Ready to resume the saved task",
        TaskResumeDisposition::RetryUncertainModelStep => {
            "The last model request has an unknown outcome. Retrying sends a new request and may incur another charge."
        }
        TaskResumeDisposition::NotInterrupted => "This task is outside the interrupted state.",
        TaskResumeDisposition::RemoteContinuation => {
            "A remote Agent owns this task. Check its status before continuing."
        }
        TaskResumeDisposition::AgentAlreadyStarted => {
            "The Agent already started this task. Verify its outcome before taking action."
        }
        TaskResumeDisposition::AgentUnavailable => "The saved Agent is unavailable.",
        TaskResumeDisposition::AgentRegistrationChanged => {
            "The current Agent registration differs from the saved task."
        }
        TaskResumeDisposition::ToolOutcomeUnknown => {
            "The last tool call may have taken effect. Verify the outcome before acting again."
        }
        TaskResumeDisposition::ModelStepBudgetExhausted => {
            "The saved task has used its model step budget."
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ReconcileField {
    Outcome,
    Operator,
    Evidence,
}

pub struct ReconcileForm {
    pub task_id: String,
    pub state: Option<AgentTaskState>,
    pub field: ReconcileField,
    pub operator: Editor,
    pub evidence: Editor,
}

impl ReconcileForm {
    pub fn new(task_id: String, state: Option<AgentTaskState>) -> Self {
        Self {
            task_id,
            state,
            field: ReconcileField::Outcome,
            operator: Editor::default(),
            evidence: Editor::default(),
        }
    }

    pub fn next_field(&mut self) {
        self.field = match self.field {
            ReconcileField::Outcome => ReconcileField::Operator,
            ReconcileField::Operator => ReconcileField::Evidence,
            ReconcileField::Evidence => ReconcileField::Outcome,
        };
    }

    pub fn insert(&mut self, value: &str) {
        match self.field {
            ReconcileField::Outcome => {}
            ReconcileField::Operator => {
                let value = value.replace(['\r', '\n'], " ");
                let _ = self.operator.insert(&value, 128);
            }
            ReconcileField::Evidence => {
                let _ = self.evidence.insert(value, 2048);
            }
        }
    }

    pub fn key(&mut self, key: KeyCode) {
        if let KeyCode::Char(character) = key
            && self.field != ReconcileField::Outcome
            && !character.is_control()
        {
            self.insert(character.encode_utf8(&mut [0; 4]));
            return;
        }
        match self.field {
            ReconcileField::Outcome
                if matches!(key, KeyCode::Left | KeyCode::Right | KeyCode::Char(' ')) =>
            {
                self.state = Some(match self.state {
                    None | Some(AgentTaskState::Rejected) => AgentTaskState::Completed,
                    Some(AgentTaskState::Completed) => AgentTaskState::Failed,
                    Some(AgentTaskState::Cancelled) => AgentTaskState::Rejected,
                    _ => AgentTaskState::Cancelled,
                });
            }
            ReconcileField::Outcome => {}
            ReconcileField::Operator => {
                self.operator.key(key);
            }
            ReconcileField::Evidence => {
                self.evidence.key(key);
            }
        }
    }

    pub fn params(&self) -> Result<TaskReconcileParams, &'static str> {
        let state = self.state.ok_or("Choose the verified outcome")?;
        let operator = self.operator.text().trim();
        let evidence = self.evidence.text().trim();
        if operator.is_empty() || evidence.is_empty() {
            return Err("Enter the operator name and the evidence you checked");
        }
        Ok(TaskReconcileParams {
            task_id: self.task_id.clone(),
            state,
            operator: operator.into(),
            evidence: evidence.into(),
            check_id: None,
        })
    }
}

pub fn outcome_label(state: AgentTaskState) -> &'static str {
    match state {
        AgentTaskState::Completed => "Completed",
        AgentTaskState::Failed => "Failed",
        AgentTaskState::Cancelled => "Cancelled",
        AgentTaskState::Rejected => "Rejected",
        _ => "Unknown",
    }
}
