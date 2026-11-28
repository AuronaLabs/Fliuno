use super::*;
use fliuno_core::{ModelDecision, ModelProvider, ModelTurn};
use fliuno_protocol::{ApprovalDecision, Message, Task, TaskStatus, Workspace};
use std::{fs, path::PathBuf};

struct Project(PathBuf);

impl Project {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("fliuno-find-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&root).unwrap();
        Self(root)
    }

    fn search(
        &self,
        arguments: Value,
        cancellation: &CancellationToken,
    ) -> Result<Value, ToolError> {
        let workspace = Workspace {
            id: "workspace".into(),
            root: self.0.to_string_lossy().into(),
        };
        let context = ToolContext {
            session_id: "session",
            task_id: "task",
            workspace: &workspace,
        };
        Find.execute(
            &context,
            &ToolCall {
                id: "find".into(),
                name: "workspace_find".into(),
                arguments,
            },
            cancellation,
        )
        .map(|result| result.output)
    }
}

impl Drop for Project {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}

#[test]
fn project_search_reports_hits_and_limits_without_reading_hidden_or_generated_content() {
    let project = Project::new();
    for (path, text) in [
        ("src/logic.rs", "first\n中文 needle\n"),
        ("tests/logic.rs", "needle"),
        (".git/config.txt", "needle"),
        ("target/generated.rs", "needle"),
        ("other.bin", "needle"),
    ] {
        let path = project.0.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }
    let token = CancellationToken::new();
    let complete = project
        .search(json!({"path":".","mode":"text","query":"needle"}), &token)
        .unwrap();
    assert_eq!(complete["examinedTextFiles"], 2);
    assert_eq!(complete["truncated"], false);
    assert_eq!(complete["matches"].as_array().unwrap().len(), 2);
    assert!(
        complete["matches"]
            .as_array()
            .unwrap()
            .contains(&json!({"path":"src/logic.rs","line":2,"byteColumn":8}))
    );
    let limited = project
        .search(
            json!({"path":".","mode":"name","query":"logic","limit":1}),
            &token,
        )
        .unwrap();
    assert_eq!(limited["matches"].as_array().unwrap().len(), 1);
    assert_eq!(limited["truncated"], true);
    for path in ["../", ".git", "target"] {
        assert!(
            project
                .search(json!({"path":path,"mode":"text","query":"needle"}), &token)
                .is_err()
        );
    }
    token.cancel();
    assert!(
        project
            .search(json!({"path":".","mode":"text","query":"needle"}), &token)
            .is_err()
    );
}

#[cfg(unix)]
#[test]
fn project_search_does_not_follow_symlink_entries_or_scope_components() {
    use std::os::unix::fs::symlink;
    let project = Project::new();
    let sibling = Project::new();
    fs::write(sibling.0.join("private.txt"), "secret needle").unwrap();
    symlink(&sibling.0, project.0.join("linked")).unwrap();
    symlink(sibling.0.join("private.txt"), project.0.join("alias.txt")).unwrap();
    let token = CancellationToken::new();
    let result = project
        .search(json!({"path":".","mode":"text","query":"needle"}), &token)
        .unwrap();
    assert_eq!(result["matches"], json!([]));
    assert_eq!(result["examinedTextFiles"], 0);
    assert!(
        project
            .search(
                json!({"path":"linked","mode":"name","query":"private"}),
                &token
            )
            .is_err()
    );
}

struct ProjectAgent;

impl ModelProvider for ProjectAgent {
    fn next(
        &self,
        task: &Task,
        tools: &[ToolSpec],
        _: &CancellationToken,
    ) -> Result<ModelTurn, String> {
        let last = task
            .messages
            .iter()
            .rev()
            .find_map(|message| match message {
                Message::ToolResult { output, .. } => Some(output),
                _ => None,
            });
        let (name, arguments) = match task.model_steps {
            1 => (
                "workspace_find",
                json!({"path":".","mode":"text","query":"Hello world"}),
            ),
            2 => (
                "workspace_read_text",
                json!({"path":last.unwrap()["matches"][0]["path"]}),
            ),
            3 => {
                let read = last.unwrap();
                assert_eq!(read["content"], "Hello world\n");
                (
                    "workspace_patch_text",
                    json!({"path":read["path"],"baseSha256":read["sha256"],"patch":"--- a/src/greeting.txt\n+++ b/src/greeting.txt\n@@ -1 +1 @@\n-Hello world\n+你好，世界\n"}),
                )
            }
            4 => ("workspace_read_text", json!({"path":last.unwrap()["path"]})),
            5 => {
                assert_eq!(last.unwrap()["content"], "你好，世界\n");
                return Ok(
                    ModelDecision::Complete("已修改并重新读取 src/greeting.txt".into()).into(),
                );
            }
            step => panic!("unexpected model step {step}"),
        };
        assert!(tools.iter().any(|tool| tool.name == name));
        Ok(ModelDecision::Call(ToolCall {
            id: format!("step-{}", task.model_steps),
            name: name.into(),
            arguments,
        })
        .into())
    }
}

#[test]
fn agent_can_discover_read_patch_and_verify_a_previously_unknown_file() {
    let project = Project::new();
    fs::create_dir(project.0.join("src")).unwrap();
    let file = project.0.join("src/greeting.txt");
    fs::write(&file, "Hello world\n").unwrap();
    let mut kernel = Kernel::in_memory(Box::new(ProjectAgent)).unwrap();
    register(&mut kernel).unwrap();
    let workspace = kernel.open_workspace(project.0.to_str().unwrap()).unwrap();
    let session = kernel.create_session(&workspace.id).unwrap();
    let mut task = kernel
        .start_task(&session.id, "把项目里输出的 Hello world 改成中文")
        .unwrap();
    for name in [
        "workspace_find",
        "workspace_read_text",
        "workspace_patch_text",
        "workspace_read_text",
    ] {
        let pending = task.pending_approval.as_ref().unwrap();
        assert_eq!(pending.call.name, name);
        if name == "workspace_patch_text" {
            assert_eq!(fs::read_to_string(&file).unwrap(), "Hello world\n");
        }
        task = kernel
            .respond_approval(&task.id, &pending.id, ApprovalDecision::AllowOnce)
            .unwrap();
    }
    assert_eq!(task.status, TaskStatus::Completed);
    assert_eq!(fs::read_to_string(&file).unwrap(), "你好，世界\n");
    assert!(task.output.unwrap().contains("src/greeting.txt"));
}
