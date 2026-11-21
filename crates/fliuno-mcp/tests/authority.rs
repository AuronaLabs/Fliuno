//! Stable aliases must not carry consent across endpoint or schema changes.
#[path = "support/http.rs"]
mod wire;
use fliuno_core::{CancellationToken, Kernel, ModelDecision, ModelProvider, ModelTurn, ToolSpec};
use fliuno_protocol::{
    ApprovalDecision, Message, Task, TaskResumeDisposition, TaskStatus, ToolCall,
};
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
fn register(kernel: &mut Kernel, fixture: &Fixture, pinned: bool) {
    let mut server = json!({
        "id":"remote","transport":"streamable_http","url":fixture.url("/mcp"),"timeoutMs":2000
    });
    if pinned {
        let port = reqwest::Url::parse(&fixture.url("/mcp"))
            .unwrap()
            .port()
            .unwrap();
        server["egress"] =
            json!({"routes":[{"host":"127.0.0.1","addresses":["127.0.0.1"],"ports":[port]}]});
    }
    fliuno_mcp::register_config(
        kernel,
        serde_json::from_value(json!({"servers":[server]})).unwrap(),
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
        let revision = schema_revision.load(Ordering::SeqCst);
        if request.body["method"] == "tools/list" && revision >= 2 {
            let mut tool = json!({"name":"echo","description":"Fixture echo",
                "inputSchema":{"type":"object","properties":{"text":{"type":"string","maxLength":100}},"required":["text"],"additionalProperties":false}});
            if revision >= 3 {
                tool["outputSchema"] = json!({"type":"array","items":{"type":"string"}});
            }
            return Response::json(
                200,
                json!({"jsonrpc":"2.0","id":request.body["id"],"result":{"tools":[tool]}}),
            );
        }
        if request.body["method"] == "tools/call" && revision >= 3 {
            let output = if revision == 3 {
                json!(["valid output"])
            } else {
                json!({"secret":"INVALID_TOOL_RESULT"})
            };
            return Response::json(
                200,
                json!({"jsonrpc":"2.0","id":request.body["id"],
                "result":{"structuredContent":output}}),
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
    register(&mut kernel, &first, false);
    let workspace = kernel.open_workspace(directory.to_str().unwrap()).unwrap();
    let session = kernel.create_session(&workspace.id).unwrap();
    let waiting = kernel.start_task(&session.id, "original endpoint").unwrap();
    let alias = waiting.pending_approval.as_ref().unwrap().call.name.clone();
    drop(kernel);
    let mut kernel = Kernel::open(&database, Box::new(Provider)).unwrap();
    register(&mut kernel, &second, false);
    assert!(kernel.tool_specs().iter().any(|tool| tool.name == alias));
    assert_eq!(approve(&mut kernel, &waiting).status, TaskStatus::Failed);
    let waiting = kernel.start_task(&session.id, "original schema").unwrap();
    drop(kernel);
    revision.store(2, Ordering::SeqCst);
    let mut kernel = Kernel::open(&database, Box::new(Provider)).unwrap();
    register(&mut kernel, &second, false);
    assert_eq!(approve(&mut kernel, &waiting).status, TaskStatus::Failed);
    let waiting = kernel
        .start_task(&session.id, "original output contract")
        .unwrap();
    drop(kernel);
    revision.store(3, Ordering::SeqCst);
    let mut kernel = Kernel::open(&database, Box::new(Provider)).unwrap();
    register(&mut kernel, &second, false);
    assert_eq!(approve(&mut kernel, &waiting).status, TaskStatus::Failed);
    let waiting = kernel.start_task(&session.id, "old egress policy").unwrap();
    drop(kernel);
    let mut kernel = Kernel::open(&database, Box::new(Provider)).unwrap();
    register(&mut kernel, &second, true);
    assert!(kernel.tool_specs().iter().any(|tool| tool.name == alias));
    assert_eq!(approve(&mut kernel, &waiting).status, TaskStatus::Failed);
    assert!(
        !second
            .requests()
            .iter()
            .any(|r| r.body["method"] == "tools/call")
    );
    let fresh = kernel.start_task(&session.id, "fresh consent").unwrap();
    assert_eq!(approve(&mut kernel, &fresh).status, TaskStatus::Completed);
    revision.store(4, Ordering::SeqCst);
    let invalid = kernel.start_task(&session.id, "invalid output").unwrap();
    let interrupted = approve(&mut kernel, &invalid);
    assert_eq!(interrupted.status, TaskStatus::Interrupted);
    assert!(interrupted.in_flight_tool.is_some());
    assert!(
        !serde_json::to_string(&interrupted)
            .unwrap()
            .contains("INVALID_TOOL_RESULT")
    );
    drop(kernel);
    let mut kernel = Kernel::open(&database, Box::new(Provider)).unwrap();
    register(&mut kernel, &second, true);
    assert_eq!(
        kernel
            .get_task_resume_status(&invalid.id)
            .unwrap()
            .disposition,
        TaskResumeDisposition::ToolOutcomeUnknown
    );
    assert!(kernel.resume_task(&invalid.id).is_err());
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
        2
    );
    std::fs::remove_dir_all(directory).unwrap();
}
