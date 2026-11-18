//! Stable aliases must not carry consent across endpoint or schema changes.
#[path = "support/http.rs"]
mod wire;
use fliuno_core::{CancellationToken, Kernel, ModelDecision, ModelProvider, ModelTurn, ToolSpec};
use fliuno_protocol::{ApprovalDecision, Message, Task, TaskStatus, ToolCall};
use serde_json::json;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use wire::{Fixture, Response, Version};

struct Provider;
impl ModelProvider for Provider {
    fn next(
        &self,
        task: &Task,
        tools: &[ToolSpec],
        _: &CancellationToken,
    ) -> Result<ModelTurn, String> {
        Ok(if task
            .messages
            .iter()
            .any(|message| matches!(message, Message::ToolResult { .. }))
        {
            ModelDecision::Complete("observed".into())
        } else {
            ModelDecision::Call(ToolCall {
                id: format!("call_{}", task.id),
                name: tools
                    .iter()
                    .find(|tool| tool.name.starts_with("mcp_remote_"))
                    .unwrap()
                    .name
                    .clone(),
                arguments: json!({"text":"consented"}),
            })
        }
        .into())
    }
}
fn register(kernel: &mut Kernel, fixture: &Fixture) {
    fliuno_mcp::register_config(
        kernel,
        serde_json::from_value(json!({"servers":[{
            "id":"remote","transport":"streamable_http","url":fixture.url("/mcp"),"timeoutMs":2000
        }]}))
        .unwrap(),
    )
    .unwrap();
}
fn approve(kernel: &mut Kernel, task: &Task) -> Task {
    kernel
        .respond_approval(
            &task.id,
            &task.pending_approval.as_ref().unwrap().id,
            ApprovalDecision::AllowOnce,
        )
        .unwrap()
}
#[test]
fn endpoint_and_schema_changes_cannot_claim_old_consent_with_the_same_alias() {
    let first = Fixture::start(|request| wire::reply(request, Version::Modern));
    let revision = Arc::new(AtomicUsize::new(1));
    let schema_revision = Arc::clone(&revision);
    let second = Fixture::start(move |request| {
        if request.body["method"] == "tools/list" && schema_revision.load(Ordering::SeqCst) == 2 {
            return Response::json(
                200,
                json!({"jsonrpc":"2.0","id":request.body["id"],"result":{"tools":[{
                    "name":"echo","description":"Fixture echo","inputSchema":{"type":"object","properties":{"text":{"type":"string","maxLength":100}},"required":["text"],"additionalProperties":false}
                }]}}),
            );
        }
        wire::reply(request, Version::Modern)
    });
    let directory = std::env::temp_dir().join(format!(
        "fliuno-http-identity-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir(&directory).unwrap();
    let database = directory.join("kernel.db");
    let mut kernel = Kernel::open(&database, Box::new(Provider)).unwrap();
    register(&mut kernel, &first);
    let workspace = kernel.open_workspace(directory.to_str().unwrap()).unwrap();
    let session = kernel.create_session(&workspace.id).unwrap();
    let waiting = kernel.start_task(&session.id, "original endpoint").unwrap();
    let alias = waiting.pending_approval.as_ref().unwrap().call.name.clone();
    drop(kernel);
    let mut kernel = Kernel::open(&database, Box::new(Provider)).unwrap();
    register(&mut kernel, &second);
    assert!(kernel.tool_specs().iter().any(|tool| tool.name == alias));
    assert_eq!(approve(&mut kernel, &waiting).status, TaskStatus::Failed);
    let waiting = kernel.start_task(&session.id, "original schema").unwrap();
    drop(kernel);
    revision.store(2, Ordering::SeqCst);
    let mut kernel = Kernel::open(&database, Box::new(Provider)).unwrap();
    register(&mut kernel, &second);
    assert_eq!(approve(&mut kernel, &waiting).status, TaskStatus::Failed);
    let fresh = kernel.start_task(&session.id, "fresh consent").unwrap();
    assert_eq!(approve(&mut kernel, &fresh).status, TaskStatus::Completed);
    drop(kernel);
    assert!(
        !first
            .requests()
            .iter()
            .any(|request| request.body["method"] == "tools/call")
    );
    assert_eq!(
        second
            .requests()
            .iter()
            .filter(|request| request.body["method"] == "tools/call")
            .count(),
        1
    );
    std::fs::remove_dir_all(directory).unwrap();
}
