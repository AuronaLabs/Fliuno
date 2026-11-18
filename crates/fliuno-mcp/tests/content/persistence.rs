//! New content capabilities use the same persisted one-shot approval identity.
use super::*;
use fliuno_core::{Kernel, ModelDecision, ModelProvider, ModelTurn, ToolSpec};
use fliuno_protocol::{ApprovalDecision, Message, Task, TaskStatus};

struct Provider {
    label: &'static str,
    arguments: Value,
}
impl ModelProvider for Provider {
    fn next(
        &self,
        task: &Task,
        _: &[ToolSpec],
        _: &CancellationToken,
    ) -> Result<ModelTurn, String> {
        Ok(if task
            .messages
            .iter()
            .any(|message| matches!(message, Message::ToolResult { .. }))
        {
            ModelDecision::Complete("content observed".into())
        } else {
            ModelDecision::Call(ToolCall {
                id: format!("call_{}", task.id),
                name: format!("mcp_remote_{}", self.label),
                arguments: self.arguments.clone(),
            })
        }
        .into())
    }
}
fn register(kernel: &mut Kernel, fixture: &Fixture) {
    fliuno_mcp::register_config(
        kernel,
        serde_json::from_value(json!({"servers":[{
            "id":"remote","transport":"streamable_http","url":fixture.url("/mcp"),"timeoutMs":2000,
        }]}))
        .unwrap(),
    )
    .unwrap();
}
fn approve(kernel: &mut Kernel, task: &Task, decision: ApprovalDecision) -> Task {
    kernel
        .respond_approval(
            &task.id,
            &task.pending_approval.as_ref().unwrap().id,
            decision,
        )
        .unwrap()
}

#[test]
fn content_consent_is_source_bound_denial_sends_nothing_and_roles_stay_in_tool_data() {
    for (label, method, arguments) in [
        (
            "resources_read",
            "resources/read",
            json!({"uri":"fixture:first"}),
        ),
        (
            "prompts_get",
            "prompts/get",
            json!({"name":"draft","arguments":{"topic":"工作成果"}}),
        ),
    ] {
        let first = Fixture::start(content::reply);
        let second = Fixture::start(content::reply);
        let directory = std::env::temp_dir().join(format!(
            "fliuno-content-approval-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&directory).unwrap();
        let database = directory.join("kernel.db");
        let provider = || {
            Box::new(Provider {
                label,
                arguments: arguments.clone(),
            }) as Box<dyn ModelProvider>
        };
        let mut kernel = Kernel::open(&database, provider()).unwrap();
        register(&mut kernel, &first);
        let workspace = kernel.open_workspace(directory.to_str().unwrap()).unwrap();
        let session = kernel.create_session(&workspace.id).unwrap();
        let original = kernel.start_task(&session.id, "original consent").unwrap();
        assert_eq!(count(&first, method), 0);
        drop(kernel);
        let mut kernel = Kernel::open(&database, provider()).unwrap();
        register(&mut kernel, &second);
        assert_eq!(
            approve(&mut kernel, &original, ApprovalDecision::AllowOnce).status,
            TaskStatus::Failed
        );
        assert_eq!(count(&second, method), 0);
        let denied = kernel.start_task(&session.id, "deny").unwrap();
        assert_eq!(
            approve(&mut kernel, &denied, ApprovalDecision::Deny).status,
            TaskStatus::Failed
        );
        assert_eq!(count(&second, method), 0);
        let fresh = kernel.start_task(&session.id, "fresh consent").unwrap();
        let done = approve(&mut kernel, &fresh, ApprovalDecision::AllowOnce);
        assert_eq!(done.status, TaskStatus::Completed);
        assert_eq!(count(&second, method), 1);
        assert_eq!(done.messages.len(), 4);
        assert!(matches!(&done.messages[0], Message::User {text} if text == "fresh consent"));
        assert!(matches!(&done.messages[1], Message::ToolCall { .. }));
        let Message::ToolResult { output, .. } = &done.messages[2] else {
            panic!("content became another message role")
        };
        assert_eq!(output["method"], method);
        assert_eq!(output["grantsPermissions"], false);
        if label == "prompts_get" {
            assert_eq!(output["result"]["messages"][0]["role"], "user");
            assert_eq!(
                output["result"]["messages"][1]["content"]["text"],
                "工作成果"
            );
        }
        let original_events =
            serde_json::to_value(kernel.list_events(&done.id, 0, 100).unwrap()).unwrap();
        drop(kernel);
        let kernel = Kernel::open(&database, provider()).unwrap();
        assert_eq!(
            serde_json::to_value(kernel.get_task(&done.id).unwrap()).unwrap(),
            serde_json::to_value(done).unwrap()
        );
        assert_eq!(
            serde_json::to_value(kernel.list_events(&fresh.id, 0, 100).unwrap()).unwrap(),
            original_events
        );
        assert_eq!(count(&first, method), 0);
        assert_eq!(count(&second, method), 1);
        drop(kernel);
        std::fs::remove_dir_all(directory).unwrap();
    }
}
