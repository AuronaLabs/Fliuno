use super::super::*;
use crate::{
    CancellationToken, Error, ModelDecision, ModelProvider, ModelTurn, ToolContext, ToolEffect,
    ToolError, ToolOutput, ToolSpec, authority_digest,
};
use fliuno_protocol::{
    ApprovalDecision, EventKind, Message, Task, TaskStatus, ToolCall, ToolInputRequest,
    ToolInputRespondParams,
};
use rusqlite::params;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    fs,
    sync::{Arc, Mutex},
};

const PRIVATE_STATE: &str = "private-mrtr-continuation-state";

#[derive(Debug, PartialEq)]
struct ResumeRecord {
    request_state: Option<String>,
    call: ToolCall,
    responses: Value,
}

struct InputTool {
    label: &'static str,
    resumed: Arc<Mutex<Option<ResumeRecord>>>,
}

impl Tool for InputTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "input_tool".into(),
            description: "Tool input continuation fixture".into(),
            effect: ToolEffect::External,
            input_schema: json!({"type":"object","required":["target"],"properties":{"target":{"type":"string"}},"additionalProperties":false}),
        }
    }

    fn authority(&self) -> String {
        authority_digest("test.input-tool.v1", &json!(self.label))
    }

    fn validate(&self, args: &Value) -> std::result::Result<(), String> {
        (args == &json!({"target":"service"}))
            .then_some(())
            .ok_or_else(|| "invalid fixture arguments".into())
    }

    fn execute(
        &self,
        _: &ToolContext<'_>,
        _: &ToolCall,
        _: &CancellationToken,
    ) -> std::result::Result<ToolOutput, ToolError> {
        Err(ToolError::InputRequired {
            requests: json!({"entry":{"label":"Confirmation code"}}),
            request_state: Some(PRIVATE_STATE.into()),
        })
    }

    fn validate_input_request(
        &self,
        request: &ToolInputRequest,
        state: Option<&str>,
    ) -> std::result::Result<(), String> {
        (request.requests == json!({"entry":{"label":"Confirmation code"}})
            && state == Some(PRIVATE_STATE))
        .then_some(())
        .ok_or_else(|| "unexpected fixture continuation".into())
    }

    fn validate_input(
        &self,
        _: &ToolInputRequest,
        responses: &Value,
    ) -> std::result::Result<(), String> {
        (responses == &json!({"entry":{"code":"1234"}}))
            .then_some(())
            .ok_or_else(|| "invalid fixture response".into())
    }

    fn resume(
        &self,
        _: &ToolContext<'_>,
        call: &ToolCall,
        state: Option<&str>,
        responses: &Value,
        _: &CancellationToken,
    ) -> std::result::Result<ToolOutput, ToolError> {
        *self.resumed.lock().unwrap() = Some(ResumeRecord {
            request_state: state.map(str::to_owned),
            call: call.clone(),
            responses: responses.clone(),
        });
        Ok(ToolOutput {
            output: json!({"accepted":true}),
            artifacts: vec![],
        })
    }
}

struct InputProvider;
impl ModelProvider for InputProvider {
    fn next(
        &self,
        task: &Task,
        _: &[ToolSpec],
        _: &CancellationToken,
    ) -> std::result::Result<ModelTurn, String> {
        if task
            .messages
            .iter()
            .any(|message| matches!(message, Message::ToolResult { .. }))
        {
            Ok(ModelDecision::Complete("input accepted".into()).into())
        } else {
            Ok(ModelDecision::Call(ToolCall {
                id: "input-call".into(),
                name: "input_tool".into(),
                arguments: json!({"target":"service"}),
            })
            .into())
        }
    }
}

fn tool(label: &'static str, resumed: &Arc<Mutex<Option<ResumeRecord>>>) -> Box<dyn Tool> {
    Box::new(InputTool {
        label,
        resumed: Arc::clone(resumed),
    })
}

fn waiting(kernel: &mut Kernel, session_id: &str) -> Task {
    let task = kernel.start_task(session_id, "continue operation").unwrap();
    kernel
        .respond_approval(
            &task.id,
            &task.pending_approval.unwrap().id,
            ApprovalDecision::AllowOnce,
        )
        .unwrap()
}

#[test]
fn continuation_survives_restart_keeps_state_private_and_retries_idempotently() {
    let directory = std::env::temp_dir().join(crate::new_id("tool-input"));
    fs::create_dir(&directory).unwrap();
    let path = directory.join("kernel.db");
    let resumed = Arc::new(Mutex::new(None));
    let mut kernel = Kernel::open(&path, Box::new(InputProvider)).unwrap();
    kernel.register_tool(tool("stable", &resumed)).unwrap();
    let workspace = kernel.open_workspace(directory.to_str().unwrap()).unwrap();
    let session = kernel.create_session(&workspace.id).unwrap();
    let waiting = waiting(&mut kernel, &session.id);
    assert_eq!(waiting.status, TaskStatus::WaitingInput);
    assert_eq!(waiting.pending_tool_input.as_ref().unwrap().round, 1);
    assert!(
        !serde_json::to_string(&waiting)
            .unwrap()
            .contains(PRIVATE_STATE)
    );
    let events = kernel.list_events(&waiting.id, 0, 100).unwrap().events;
    assert!(!events.iter().any(|event| {
        serde_json::to_string(&event.data)
            .unwrap()
            .contains(PRIVATE_STATE)
    }));
    let response_hash = format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(&json!({"entry":{"code":"1234"}})).unwrap())
    );
    assert!(!events.iter().any(|event| {
        let data = serde_json::to_string(&event.data).unwrap();
        data.contains("1234") || data.contains(&response_hash)
    }));
    let saved: Option<String> = kernel
        .db
        .query_row(
            "SELECT request_state FROM tool_input_continuations WHERE task_id=?1",
            [&waiting.id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(saved.as_deref(), Some(PRIVATE_STATE));
    drop(kernel);

    let request = ToolInputRespondParams {
        task_id: waiting.id.clone(),
        call_id: "input-call".into(),
        request_id: "response-1".into(),
        responses: json!({"entry":{"code":"1234"}}),
    };
    let mut changed = Kernel::open(&path, Box::new(InputProvider)).unwrap();
    changed.register_tool(tool("changed", &resumed)).unwrap();
    assert!(matches!(
        changed.prepare_tool_input_response(&request),
        Err(Error::Conflict(_))
    ));
    drop(changed);

    let mut kernel = Kernel::open(&path, Box::new(InputProvider)).unwrap();
    kernel.register_tool(tool("stable", &resumed)).unwrap();
    assert_eq!(
        kernel.get_task(&waiting.id).unwrap().status,
        TaskStatus::WaitingInput
    );
    let ToolInputOutcome::Prepared { task, execution } =
        kernel.prepare_tool_input_response(&request).unwrap()
    else {
        panic!("first response must prepare one continuation");
    };
    assert_eq!(task.status, TaskStatus::Running);
    assert!(task.pending_tool_input.is_none());
    assert!(
        matches!(kernel.prepare_tool_input_response(&request).unwrap(),
        ToolInputOutcome::Duplicate(task) if task.status == TaskStatus::Running)
    );
    let changed_retry = ToolInputRespondParams {
        responses: json!({"entry":{"code":"9999"}}),
        ..request.clone()
    };
    assert!(matches!(
        kernel.prepare_tool_input_response(&changed_retry),
        Err(Error::Conflict(_))
    ));

    let result = execution.execute(&CancellationToken::new());
    let running = kernel.apply_tool_response(&execution, result).unwrap();
    let completed = kernel.drive(running).unwrap();
    assert_eq!(completed.status, TaskStatus::Completed);
    assert_eq!(
        resumed.lock().unwrap().as_ref().unwrap(),
        &ResumeRecord {
            request_state: Some(PRIVATE_STATE.into()),
            call: ToolCall {
                id: "input-call".into(),
                name: "input_tool".into(),
                arguments: json!({"target":"service"})
            },
            responses: json!({"entry":{"code":"1234"}}),
        }
    );
    assert!(
        kernel
            .list_events(&waiting.id, 0, 100)
            .unwrap()
            .events
            .iter()
            .any(|event| matches!(event.data, EventKind::ToolInputResolved { .. }))
    );
    drop(kernel);
    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn cancelling_waiting_input_clears_request_and_private_state() {
    let resumed = Arc::new(Mutex::new(None));
    let mut kernel = Kernel::in_memory(Box::new(InputProvider)).unwrap();
    kernel.register_tool(tool("stable", &resumed)).unwrap();
    let workspace = kernel
        .open_workspace(std::env::current_dir().unwrap().to_str().unwrap())
        .unwrap();
    let session = kernel.create_session(&workspace.id).unwrap();
    let waiting = waiting(&mut kernel, &session.id);
    assert_eq!(waiting.status, TaskStatus::WaitingInput);
    let counts = kernel
        .work_session_counts(&WorkSessionCountsParams {
            workspace_id: workspace.id.clone(),
            session_ids: vec![session.id.clone()],
        })
        .unwrap();
    assert_eq!(
        counts.items[0].as_ref().unwrap().task_counts.waiting_input,
        1
    );
    let cancelled = kernel.cancel_task(&waiting.id).unwrap();
    assert_eq!(cancelled.status, TaskStatus::Cancelled);
    assert!(cancelled.pending_tool_input.is_none());
    let state: Option<String> = kernel
        .db
        .query_row(
            "SELECT request_state FROM tool_input_continuations WHERE task_id=?1",
            params![waiting.id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(state, None);
    let counts = kernel
        .work_session_counts(&WorkSessionCountsParams {
            workspace_id: workspace.id,
            session_ids: vec![session.id],
        })
        .unwrap();
    assert_eq!(counts.items[0].as_ref().unwrap().task_counts.cancelled, 1);
    assert_eq!(
        counts.items[0].as_ref().unwrap().task_counts.waiting_input,
        0
    );
}

#[test]
fn schema_v52_upgrade_adds_private_input_storage_and_waiting_input_counters() {
    let directory = std::env::temp_dir().join(crate::new_id("tool-input-migration"));
    fs::create_dir(&directory).unwrap();
    let path = directory.join("kernel.db");
    let mut kernel = Kernel::open(&path, Box::new(crate::DemoProvider)).unwrap();
    let workspace = kernel.open_workspace(directory.to_str().unwrap()).unwrap();
    let session = kernel.create_session(&workspace.id).unwrap();
    let waiting = kernel.start_task(&session.id, "legacy task").unwrap();
    assert_eq!(waiting.status, TaskStatus::WaitingApproval);
    kernel
        .db
        .execute_batch(
            "DROP TRIGGER work_status_insert;
             DROP TRIGGER work_status_update;
             DROP TRIGGER work_status_delete;
             DROP TABLE tool_input_continuations;
             ALTER TABLE work_status_counts DROP COLUMN waiting_input;
             PRAGMA user_version=52;",
        )
        .unwrap();
    drop(kernel);

    let kernel = Kernel::open(&path, Box::new(crate::DemoProvider)).unwrap();
    let version: u32 = kernel
        .db
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .unwrap();
    assert_eq!(version, crate::storage::SCHEMA_VERSION);
    let table_exists: bool = kernel
        .db
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='tool_input_continuations')",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(table_exists);
    let counts = kernel
        .work_session_counts(&WorkSessionCountsParams {
            workspace_id: workspace.id,
            session_ids: vec![session.id],
        })
        .unwrap();
    assert_eq!(
        counts.items[0]
            .as_ref()
            .unwrap()
            .task_counts
            .waiting_approval,
        1
    );
    assert_eq!(
        counts.items[0].as_ref().unwrap().task_counts.waiting_input,
        0
    );
    drop(kernel);
    fs::remove_dir_all(directory).unwrap();
}
