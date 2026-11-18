use super::*;
use fliuno_protocol::Workspace;
use std::{
    fs,
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};

#[test]
fn collision_with_the_last_workspace_tool_does_not_register_preceding_tools() {
    let mut kernel = Kernel::in_memory(Box::new(fliuno_core::DemoProvider)).unwrap();
    kernel
        .register_tool(Box::new(WorkspaceTool::new(
            Operation::Replace,
            Arc::new(Mutex::new(())),
        )))
        .unwrap();
    let before: Vec<_> = kernel
        .tool_specs()
        .into_iter()
        .map(|tool| tool.name)
        .collect();
    assert!(register(&mut kernel).is_err());
    assert_eq!(
        kernel
            .tool_specs()
            .into_iter()
            .map(|tool| tool.name)
            .collect::<Vec<_>>(),
        before
    );
}

#[test]
fn full_text_proposals_create_nothing_before_execution_and_reject_changed_bases() {
    let root = std::env::temp_dir().join(format!(
        "fliuno-replace-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir(&root).unwrap();
    let cancel = CancellationToken::new();
    let workspace = Workspace {
        id: "workspace".into(),
        root: root.to_str().unwrap().into(),
    };
    let context = ToolContext {
        session_id: "session_test",
        task_id: "task",
        workspace: &workspace,
    };
    let tool = WorkspaceTool::new(Operation::Replace, Arc::new(Mutex::new(())));
    let proposal =
        prepare_workspace_write(&root, "create", "new.txt", "new content\n", &cancel).unwrap();
    assert!(!root.join("new.txt").exists());
    assert_eq!(proposal.preview["createsFile"], true);
    assert!(
        proposal.preview["diff"]
            .as_str()
            .unwrap()
            .contains("+new content")
    );
    fs::write(root.join("new.txt"), "appeared after review").unwrap();
    assert!(matches!(
        tool.execute(&context, &proposal.call, &cancel),
        Err(ToolError::Failed(_))
    ));
    assert_eq!(
        fs::read_to_string(root.join("new.txt")).unwrap(),
        "appeared after review"
    );
    let proposal =
        prepare_workspace_write(&root, "replace", "new.txt", "short\n", &cancel).unwrap();
    fs::write(root.join("new.txt"), "changed after review").unwrap();
    assert!(matches!(
        tool.execute(&context, &proposal.call, &cancel),
        Err(ToolError::Failed(_))
    ));
    assert_eq!(
        fs::read_to_string(root.join("new.txt")).unwrap(),
        "changed after review"
    );
    let proposal =
        prepare_workspace_write(&root, "replace-again", "new.txt", "short\n", &cancel).unwrap();
    tool.execute(&context, &proposal.call, &cancel).unwrap();
    assert_eq!(fs::read_to_string(root.join("new.txt")).unwrap(), "short\n");
    fs::hard_link(root.join("new.txt"), root.join("alias.txt")).unwrap();
    assert!(prepare_workspace_write(&root, "linked", "new.txt", "unsafe", &cancel).is_err());
    assert!(prepare_workspace_write(&root, "escape", "../outside.txt", "unsafe", &cancel).is_err());
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn directory_pages_keep_order_and_cursors_beyond_the_legacy_entry_limit() {
    let root = std::env::temp_dir().join(format!(
        "fliuno-directory-page-{}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let directory = root.join("entries");
    fs::create_dir_all(&directory).unwrap();
    let names: Vec<_> = (0..305).map(|i| format!("报告-{i:03}.txt")).collect();
    for name in names.iter().rev() {
        fs::write(directory.join(name), []).unwrap();
    }
    fs::create_dir(directory.join("最后")).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;
        fs::write(directory.join(std::ffi::OsString::from_vec(vec![0xff])), []).unwrap();
    }
    let workspace = Workspace {
        id: "workspace".into(),
        root: root.to_str().unwrap().into(),
    };
    let context = ToolContext {
        session_id: "session",
        task_id: "task",
        workspace: &workspace,
    };
    let tool = directory_page::ListPage;
    for invalid in [
        json!({"path":"../outside"}),
        json!({"path":"entries","after":"nested/file"}),
        json!({"path":"entries","limit":257}),
    ] {
        assert!(tool.validate(&invalid).is_err());
    }
    let cancel = CancellationToken::new();
    let mut found = Vec::new();
    let mut after: Option<String> = None;
    for expected_size in [127, 127, 52] {
        let arguments = json!({"path":"entries","after":after,"limit":127});
        let call = ToolCall {
            id: "directory-page".into(),
            name: tool.spec().name,
            arguments,
        };
        let page = tool.execute(&context, &call, &cancel).unwrap().output;
        assert_eq!(page["omittedNames"], usize::from(cfg!(unix)));
        let entries = page["entries"].as_array().unwrap();
        assert_eq!(entries.len(), expected_size);
        let last = &entries.last().unwrap()["name"];
        if expected_size == 127 {
            assert_eq!(page["hasMore"], true);
            assert_eq!(page["nextAfter"], *last);
        } else {
            assert_eq!(page["hasMore"], false);
            assert!(page["nextAfter"].is_null());
            assert_eq!(entries.last().unwrap()["kind"], "directory");
        }
        after = page["nextAfter"].as_str().map(str::to_owned);
        found.extend(
            entries
                .iter()
                .map(|entry| entry["name"].as_str().unwrap().to_owned()),
        );
    }
    let mut expected = names;
    expected.push("最后".into());
    assert_eq!(found, expected);
    cancel.cancel();
    let cancelled = ToolCall {
        id: "cancelled".into(),
        name: tool.spec().name,
        arguments: json!({"path":"entries"}),
    };
    assert!(tool.execute(&context, &cancelled, &cancel).is_err());
    for entry in fs::read_dir(&directory).unwrap() {
        let entry = entry.unwrap();
        if entry.file_type().unwrap().is_dir() {
            fs::remove_dir(entry.path()).unwrap();
        } else {
            fs::remove_file(entry.path()).unwrap();
        }
    }
    fs::remove_dir(directory).unwrap();
    fs::remove_dir(root).unwrap();
}

struct DirectoryPageProvider;

impl fliuno_core::ModelProvider for DirectoryPageProvider {
    fn next(
        &self,
        task: &fliuno_protocol::Task,
        _tools: &[ToolSpec],
        _cancellation: &CancellationToken,
    ) -> Result<fliuno_core::ModelTurn, String> {
        use fliuno_protocol::Message;
        let last_page = task
            .messages
            .iter()
            .rev()
            .find_map(|message| match message {
                Message::ToolResult { output, .. } => Some(output),
                _ => None,
            });
        let after = last_page.and_then(|output| output["nextAfter"].as_str());
        if last_page.is_some() && after.is_none() {
            return Ok(fliuno_core::ModelDecision::Complete("listed the directory".into()).into());
        }
        Ok(fliuno_core::ModelDecision::Call(ToolCall {
            id: format!("page-{}", task.model_steps),
            name: "workspace_list_page".into(),
            arguments: json!({"path":".","after":after,"limit":1}),
        })
        .into())
    }
}

#[test]
fn each_directory_page_requires_a_separate_approval() {
    use fliuno_protocol::{ApprovalDecision, Message, TaskStatus};
    let root = std::env::temp_dir().join(format!(
        "fliuno-page-approval-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir(&root).unwrap();
    for name in ["a.txt", "b.txt"] {
        fs::write(root.join(name), []).unwrap();
    }
    let mut kernel = Kernel::in_memory(Box::new(DirectoryPageProvider)).unwrap();
    register(&mut kernel).unwrap();
    let workspace = kernel.open_workspace(root.to_str().unwrap()).unwrap();
    let session = kernel.create_session(&workspace.id).unwrap();
    let task = kernel.start_task(&session.id, "inspect directory").unwrap();
    let first = task.pending_approval.as_ref().unwrap();
    assert_eq!(first.call.name, "workspace_list_page");
    assert!(
        task.messages
            .iter()
            .all(|message| !matches!(message, Message::ToolResult { .. }))
    );
    let waiting = kernel
        .respond_approval(&task.id, &first.id, ApprovalDecision::AllowOnce)
        .unwrap();
    let second = waiting.pending_approval.as_ref().unwrap();
    assert_ne!(second.id, first.id);
    assert_eq!(second.call.arguments["after"], "a.txt");
    let complete = kernel
        .respond_approval(&task.id, &second.id, ApprovalDecision::AllowOnce)
        .unwrap();
    assert_eq!(complete.status, TaskStatus::Completed);
    let names: Vec<_> = complete
        .messages
        .iter()
        .filter_map(|message| match message {
            Message::ToolResult { output, .. } => output["entries"][0]["name"].as_str(),
            _ => None,
        })
        .collect();
    assert_eq!(names, ["a.txt", "b.txt"]);
    for name in names {
        fs::remove_file(root.join(name)).unwrap();
    }
    fs::remove_dir(root).unwrap();
}

#[test]
fn ranged_reads_preserve_line_endings_and_hash_the_full_file() {
    let root = std::env::temp_dir().join(format!(
        "fliuno-read-range-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir(&root).unwrap();
    let text = "one\r\n你好\r\nlast";
    fs::write(root.join("input.txt"), text).unwrap();
    let workspace = Workspace {
        id: "workspace".into(),
        root: root.to_str().unwrap().into(),
    };
    let context = ToolContext {
        session_id: "session_test",
        task_id: "task",
        workspace: &workspace,
    };
    let tool = WorkspaceTool::new(Operation::Read, Arc::new(Mutex::new(())));
    let call = ToolCall {
        id: "read".into(),
        name: "workspace_read_text".into(),
        arguments: json!({"path":"input.txt","line":2,"limit":1}),
    };
    let result = tool
        .execute(&context, &call, &CancellationToken::new())
        .unwrap()
        .output;
    assert_eq!(result["content"], "你好\r\n");
    assert_eq!(result["sha256"], sha256(text.as_bytes()));
    let mut beyond = call.clone();
    beyond.arguments["line"] = json!(99);
    assert_eq!(
        tool.execute(&context, &beyond, &CancellationToken::new())
            .unwrap()
            .output["content"],
        ""
    );
    beyond.arguments["line"] = json!(0);
    assert!(tool.validate(&beyond.arguments).is_err());
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn opaque_binary_reads_preserve_all_byte_values_hash_original_bytes_and_enforce_the_exact_limit() {
    use base64::{Engine, engine::general_purpose::STANDARD};
    let root = std::env::temp_dir().join(format!(
        "fliuno-binary-read-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir(&root).unwrap();
    let workspace = Workspace {
        id: "workspace".into(),
        root: root.to_str().unwrap().into(),
    };
    let context = ToolContext {
        session_id: "session_test",
        task_id: "task",
        workspace: &workspace,
    };
    let tool = WorkspaceTool::new(Operation::ReadBinary, Arc::new(Mutex::new(())));
    let call = ToolCall {
        id: "binary".into(),
        name: "workspace_read_bytes".into(),
        arguments: json!({"path":"asset.bin"}),
    };
    let token = CancellationToken::new();
    let bytes: Vec<_> = (0..=u8::MAX).collect();
    fs::write(root.join("asset.bin"), &bytes).unwrap();
    let output = tool.execute(&context, &call, &token).unwrap().output;
    assert_eq!(
        STANDARD.decode(output["data"].as_str().unwrap()).unwrap(),
        bytes
    );
    assert_eq!(output["encoding"], "base64");
    assert_eq!(output["bytes"], 256);
    assert_eq!(output["sha256"], sha256(&bytes));
    assert_eq!(output["grantsPermissions"], false);
    assert!(
        tool.spec().input_schema["required"]
            .as_array()
            .unwrap()
            .contains(&json!("path"))
    );
    assert!(
        tool.validate(&json!({"path":"asset.bin","encoding":"utf8"}))
            .is_err()
    );
    fs::write(root.join("asset.bin"), []).unwrap();
    let empty = tool.execute(&context, &call, &token).unwrap().output;
    assert_eq!(empty["data"], "");
    assert_eq!(empty["bytes"], 0);
    assert_eq!(empty["sha256"], sha256(&[]));
    let exact: Vec<_> = (0..READ_LIMIT as usize).map(|index| index as u8).collect();
    fs::write(root.join("asset.bin"), &exact).unwrap();
    let output = tool.execute(&context, &call, &token).unwrap().output;
    assert_eq!(
        STANDARD.decode(output["data"].as_str().unwrap()).unwrap(),
        exact
    );
    assert_eq!(output["bytes"], READ_LIMIT);
    fs::write(root.join("asset.bin"), vec![0; READ_LIMIT as usize + 1]).unwrap();
    assert!(tool.execute(&context, &call, &token).is_err());
    assert_eq!(
        fs::metadata(root.join("asset.bin")).unwrap().len(),
        READ_LIMIT + 1
    );
    fs::remove_dir_all(root).unwrap();
}

struct BinaryReadProvider;
impl fliuno_core::ModelProvider for BinaryReadProvider {
    fn next(
        &self,
        task: &fliuno_protocol::Task,
        _tools: &[ToolSpec],
        _cancellation: &fliuno_core::CancellationToken,
    ) -> Result<fliuno_core::ModelTurn, String> {
        use fliuno_core::ModelDecision;
        if task
            .messages
            .iter()
            .any(|message| matches!(message, fliuno_protocol::Message::ToolResult { .. }))
        {
            return Ok(ModelDecision::Complete("binary inspected".into()).into());
        }
        Ok(ModelDecision::Call(ToolCall {
            id: format!("binary-{}", task.id),
            name: "workspace_read_bytes".into(),
            arguments: json!({"path":"asset.bin"}),
        })
        .into())
    }
}

#[test]
fn binary_reads_are_approved_once_denial_returns_no_bytes_and_restart_keeps_the_reviewed_call() {
    let directory = std::env::temp_dir().join(format!(
        "fliuno-binary-approval-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir(&directory).unwrap();
    let database = directory.join("kernel.db");
    let bytes: Vec<_> = (0..=u8::MAX).rev().collect();
    fs::write(directory.join("asset.bin"), &bytes).unwrap();
    let mut kernel = Kernel::open(&database, Box::new(BinaryReadProvider)).unwrap();
    register(&mut kernel).unwrap();
    let workspace = kernel.open_workspace(directory.to_str().unwrap()).unwrap();
    let session = kernel.create_session(&workspace.id).unwrap();
    let denied = kernel.start_task(&session.id, "deny binary").unwrap();
    let approval = denied.pending_approval.as_ref().unwrap();
    assert_eq!(approval.call.name, "workspace_read_bytes");
    assert!(
        !denied
            .messages
            .iter()
            .any(|message| matches!(message, fliuno_protocol::Message::ToolResult { .. }))
    );
    let denied = kernel
        .respond_approval(
            &denied.id,
            &approval.id,
            fliuno_protocol::ApprovalDecision::Deny,
        )
        .unwrap();
    assert_eq!(denied.status, fliuno_protocol::TaskStatus::Failed);
    assert!(
        !denied
            .messages
            .iter()
            .any(|message| matches!(message, fliuno_protocol::Message::ToolResult { .. }))
    );

    let waiting = kernel.start_task(&session.id, "allow binary").unwrap();
    let approval_id = waiting.pending_approval.as_ref().unwrap().id.clone();
    let original = serde_json::to_value(&waiting).unwrap();
    drop(kernel);

    let mut kernel = Kernel::open(&database, Box::new(BinaryReadProvider)).unwrap();
    register(&mut kernel).unwrap();
    assert_eq!(
        serde_json::to_value(kernel.get_task(&waiting.id).unwrap()).unwrap(),
        original
    );
    let done = kernel
        .respond_approval(
            &waiting.id,
            &approval_id,
            fliuno_protocol::ApprovalDecision::AllowOnce,
        )
        .unwrap();
    assert_eq!(done.status, fliuno_protocol::TaskStatus::Completed);
    let output = done
        .messages
        .iter()
        .find_map(|message| match message {
            fliuno_protocol::Message::ToolResult { output, .. } => Some(output),
            _ => None,
        })
        .unwrap();
    use base64::{Engine, engine::general_purpose::STANDARD};
    assert_eq!(
        STANDARD.decode(output["data"].as_str().unwrap()).unwrap(),
        bytes
    );
    assert_eq!(output["sha256"], sha256(&bytes));
    assert_eq!(output["grantsPermissions"], false);
    let events = kernel.list_events(&done.id, 0, 100).unwrap();
    assert_eq!(
        events
            .events
            .iter()
            .filter(|event| matches!(event.data, fliuno_protocol::EventKind::ToolStarted { .. }))
            .count(),
        1
    );
    drop(kernel);
    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn portable_path_validation_rejects_escape_syntax() {
    for path in [
        "../secret",
        "a/../../secret",
        "/etc/passwd",
        "C:/Windows",
        "C:relative",
        "file:stream",
        "\\\\host\\share",
        "a\\..\\b",
        "a//b",
        "",
        "NUL",
        "con.txt",
        "COM1.log",
        "a/trailing.",
        "a/trailing ",
        "escape/",
        "line\nfeed",
    ] {
        assert!(relative(path, false).is_err(), "{path}");
    }
    assert!(relative("docs/report.md", false).is_ok());
    assert!(relative(".", true).is_ok());
}

struct WriteProvider;
impl fliuno_core::ModelProvider for WriteProvider {
    fn next(
        &self,
        task: &fliuno_protocol::Task,
        _tools: &[ToolSpec],
        _cancellation: &fliuno_core::CancellationToken,
    ) -> Result<fliuno_core::ModelTurn, String> {
        use fliuno_core::ModelDecision;
        if task
            .messages
            .iter()
            .any(|m| matches!(m, fliuno_protocol::Message::ToolResult { .. }))
        {
            Ok(ModelDecision::Complete("done".into()).into())
        } else {
            Ok(ModelDecision::Call(ToolCall {
                id: "write".into(),
                name: "workspace_create_text".into(),
                arguments: json!({"path":"created.txt","content":"approved"}),
            })
            .into())
        }
    }
}

struct PatchProvider {
    base_sha256: String,
    patch: String,
}

impl fliuno_core::ModelProvider for PatchProvider {
    fn next(
        &self,
        task: &fliuno_protocol::Task,
        _tools: &[ToolSpec],
        _cancellation: &fliuno_core::CancellationToken,
    ) -> Result<fliuno_core::ModelTurn, String> {
        use fliuno_core::ModelDecision;
        if task
            .messages
            .iter()
            .any(|message| matches!(message, fliuno_protocol::Message::ToolResult { .. }))
        {
            return Ok(ModelDecision::Complete("patch applied".into()).into());
        }
        Ok(ModelDecision::Call(ToolCall {
            id: "patch".into(),
            name: "workspace_patch_text".into(),
            arguments: json!({
                "path":"report.txt",
                "baseSha256":self.base_sha256,
                "patch":self.patch
            }),
        })
        .into())
    }
}

fn report_patch() -> &'static str {
    "--- a/report.txt\n+++ b/report.txt\n@@ -1,2 +1,2 @@\n-before\n+after\n stable\n"
}

#[test]
fn file_patch_is_shown_for_approval_and_applies_only_to_its_reviewed_version() {
    let root = std::env::temp_dir().join(format!(
        "fliuno-patch-review-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir(&root).unwrap();
    let old = "before\nstable\n";
    fs::write(root.join("report.txt"), old).unwrap();
    let mut kernel = Kernel::in_memory(Box::new(PatchProvider {
        base_sha256: sha256(old.as_bytes()),
        patch: report_patch().into(),
    }))
    .unwrap();
    register(&mut kernel).unwrap();
    let workspace = kernel.open_workspace(root.to_str().unwrap()).unwrap();
    let session = kernel.create_session(&workspace.id).unwrap();
    let task = kernel.start_task(&session.id, "edit report").unwrap();
    let approval = task.pending_approval.unwrap();
    assert_eq!(approval.call.arguments["patch"], report_patch());
    assert_eq!(fs::read_to_string(root.join("report.txt")).unwrap(), old);

    let done = kernel
        .respond_approval(
            &task.id,
            &approval.id,
            fliuno_protocol::ApprovalDecision::AllowOnce,
        )
        .unwrap();
    assert_eq!(done.status, fliuno_protocol::TaskStatus::Completed);
    assert_eq!(
        fs::read_to_string(root.join("report.txt")).unwrap(),
        "after\nstable\n"
    );
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn file_patch_rejects_stale_bases_and_multi_file_or_misdirected_diffs() {
    let root = std::env::temp_dir().join(format!(
        "fliuno-patch-guard-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir(&root).unwrap();
    let target = root.join("report.txt");
    fs::write(&target, "changed elsewhere\n").unwrap();
    let workspace = Workspace {
        id: "ws".into(),
        root: root.to_str().unwrap().into(),
    };
    let context = ToolContext {
        session_id: "session_test",
        task_id: "task",
        workspace: &workspace,
    };
    let args = json!({
        "path":"report.txt",
        "baseSha256":sha256(b"before\nstable\n"),
        "patch":report_patch()
    });
    let tool = WorkspaceTool::new(Operation::Patch, Arc::new(Mutex::new(())));
    assert!(
        tool.execute(
            &context,
            &ToolCall {
                id: "p".into(),
                name: "workspace_patch_text".into(),
                arguments: args.clone()
            },
            &fliuno_core::CancellationToken::new()
        )
        .is_err()
    );
    assert_eq!(fs::read_to_string(&target).unwrap(), "changed elsewhere\n");

    let mut multi_file = args.clone();
    multi_file["patch"] = json!(format!(
        "{}--- a/other.txt\n+++ b/other.txt\n@@ -1 +1 @@\n-old\n+new\n",
        report_patch()
    ));
    assert!(tool.validate(&multi_file).is_err());

    let mut wrong_path = args;
    wrong_path["patch"] = json!(report_patch().replace("report.txt", "../secret.txt"));
    assert!(tool.validate(&wrong_path).is_err());
    fs::remove_dir_all(root).unwrap();
}

#[cfg(any(unix, windows))]
#[test]
fn file_patch_rejects_hard_links_to_avoid_modifying_an_external_name() {
    let root = std::env::temp_dir().join(format!(
        "fliuno-hardlink-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let workspace_root = root.join("workspace");
    fs::create_dir_all(&workspace_root).unwrap();
    let target = workspace_root.join("report.txt");
    let external_name = root.join("external-link.txt");
    let original = "before\nstable\n";
    fs::write(&target, original).unwrap();
    fs::hard_link(&target, &external_name).unwrap();

    let workspace = Workspace {
        id: "ws".into(),
        root: workspace_root.to_string_lossy().into_owned(),
    };
    let context = ToolContext {
        session_id: "session_test",
        task_id: "task",
        workspace: &workspace,
    };
    let call = ToolCall {
        id: "patch".into(),
        name: "workspace_patch_text".into(),
        arguments: json!({
            "path":"report.txt",
            "baseSha256":sha256(original.as_bytes()),
            "patch":report_patch()
        }),
    };
    let tool = WorkspaceTool::new(Operation::Patch, Arc::new(Mutex::new(())));
    assert!(
        tool.execute(&context, &call, &fliuno_core::CancellationToken::new())
            .is_err()
    );
    assert_eq!(fs::read_to_string(&target).unwrap(), original);
    assert_eq!(fs::read_to_string(&external_name).unwrap(), original);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn workspace_writes_use_the_tasks_scope_and_require_approval() {
    let root = std::env::temp_dir().join(format!(
        "fliuno-tool-kernel-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let a = root.join("a");
    let b = root.join("b");
    fs::create_dir_all(&a).unwrap();
    fs::create_dir(&b).unwrap();
    let mut kernel = Kernel::in_memory(Box::new(WriteProvider)).unwrap();
    register(&mut kernel).unwrap();
    let ws = kernel.open_workspace(a.to_str().unwrap()).unwrap();
    let session = kernel.create_session(&ws.id).unwrap();
    let task = kernel.start_task(&session.id, "create file").unwrap();
    assert!(!a.join("created.txt").exists());
    kernel
        .respond_approval(
            &task.id,
            &task.pending_approval.unwrap().id,
            fliuno_protocol::ApprovalDecision::AllowOnce,
        )
        .unwrap();
    assert_eq!(
        fs::read_to_string(a.join("created.txt")).unwrap(),
        "approved"
    );
    assert!(!b.join("created.txt").exists());
    assert!(
        kernel
            .list_events(&task.id, 0, 100)
            .unwrap()
            .events
            .iter()
            .any(|e| matches!(e.data, fliuno_protocol::EventKind::ToolStarted { .. }))
    );
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn capabilities_read_create_and_prevent_overwrite() {
    let root = std::env::temp_dir().join(format!(
        "fliuno-tools-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir(&root).unwrap();
    let ws = Workspace {
        id: "ws".into(),
        root: root.to_str().unwrap().into(),
    };
    let context = ToolContext {
        session_id: "session_test",
        task_id: "task",
        workspace: &ws,
    };
    let call = ToolCall {
        id: "c".into(),
        name: "workspace_create_text".into(),
        arguments: json!({"path":"report.txt","content":"中文 report"}),
    };
    let tool = WorkspaceTool::new(Operation::Create, Arc::new(Mutex::new(())));
    let cancellation = fliuno_core::CancellationToken::new();
    tool.execute(&context, &call, &cancellation).unwrap();
    assert!(tool.execute(&context, &call, &cancellation).is_err());
    assert_eq!(
        fs::read_to_string(root.join("report.txt")).unwrap(),
        "中文 report"
    );
    let read = ToolCall {
        id: "r".into(),
        name: "workspace_read_text".into(),
        arguments: json!({"path":"report.txt"}),
    };
    let read_output = WorkspaceTool::new(Operation::Read, Arc::new(Mutex::new(())))
        .execute(&context, &read, &cancellation)
        .unwrap()
        .output;
    assert_eq!(read_output["content"], "中文 report");
    assert_eq!(read_output["sha256"], sha256("中文 report".as_bytes()));
    let list = ToolCall {
        id: "l".into(),
        name: "workspace_list".into(),
        arguments: json!({"path":"."}),
    };
    assert_eq!(
        WorkspaceTool::new(Operation::List, Arc::new(Mutex::new(())))
            .execute(&context, &list, &cancellation)
            .unwrap()
            .output["entries"][0]["name"],
        "report.txt"
    );
    fs::remove_dir_all(root).unwrap();
}

#[cfg(unix)]
#[test]
fn symlinks_cannot_escape_the_workspace() {
    let root = std::env::temp_dir().join(format!(
        "fliuno-symlink-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let inside = root.join("inside");
    let outside = root.join("outside");
    fs::create_dir_all(&inside).unwrap();
    fs::create_dir(&outside).unwrap();
    fs::write(outside.join("secret"), "private").unwrap();
    std::os::unix::fs::symlink(&outside, inside.join("escape")).unwrap();
    let ws = Workspace {
        id: "ws".into(),
        root: inside.to_str().unwrap().into(),
    };
    let context = ToolContext {
        session_id: "session_test",
        task_id: "t",
        workspace: &ws,
    };
    let stat = stat::Stat;
    let mut call = ToolCall {
        id: "s".into(),
        name: "workspace_stat".into(),
        arguments: json!({"path":"escape"}),
    };
    let token = CancellationToken::new();
    let link = stat.execute(&context, &call, &token).unwrap();
    assert_eq!(link.output["kind"], "symlink");
    assert!(link.output["bytes"].is_null());
    call.arguments["path"] = json!("escape/secret");
    assert!(stat.execute(&context, &call, &token).is_err());
    let hash = hash::Hash;
    call.name = hash.spec().name;
    assert!(hash.execute(&context, &call, &token).is_err());
    let read = ToolCall {
        id: "r".into(),
        name: "workspace_read_text".into(),
        arguments: json!({"path":"escape/secret"}),
    };
    assert!(
        WorkspaceTool::new(Operation::Read, Arc::new(Mutex::new(())))
            .execute(&context, &read, &fliuno_core::CancellationToken::new())
            .is_err()
    );
    let trailing_slash_read = ToolCall {
        id: "trailing".into(),
        name: "workspace_read_text".into(),
        arguments: json!({"path":"escape/secret/"}),
    };
    assert!(
        WorkspaceTool::new(Operation::Read, Arc::new(Mutex::new(())))
            .execute(
                &context,
                &trailing_slash_read,
                &fliuno_core::CancellationToken::new()
            )
            .is_err()
    );
    let write = ToolCall {
        id: "w".into(),
        name: "workspace_create_text".into(),
        arguments: json!({"path":"escape/new","content":"bad"}),
    };
    assert!(
        WorkspaceTool::new(Operation::Create, Arc::new(Mutex::new(())))
            .execute(&context, &write, &fliuno_core::CancellationToken::new())
            .is_err()
    );
    assert!(!outside.join("new").exists());
    fs::remove_dir_all(root).unwrap();
}

struct StatProvider;

impl fliuno_core::ModelProvider for StatProvider {
    fn next(
        &self,
        task: &fliuno_protocol::Task,
        _tools: &[ToolSpec],
        _cancellation: &CancellationToken,
    ) -> Result<fliuno_core::ModelTurn, String> {
        use fliuno_core::ModelDecision;
        if task
            .messages
            .iter()
            .any(|message| matches!(message, fliuno_protocol::Message::ToolResult { .. }))
        {
            return Ok(ModelDecision::Complete("metadata inspected".into()).into());
        }
        Ok(ModelDecision::Call(ToolCall {
            id: "stat".into(),
            name: "workspace_stat".into(),
            arguments: json!({"path":"资料.bin"}),
        })
        .into())
    }
}

#[test]
fn stat_reports_metadata_without_the_file_read_limit() {
    let root = std::env::temp_dir().canonicalize().unwrap();
    let directory = root.join(format!(
        "fliuno-stat-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir(&directory).unwrap();
    let workspace = Workspace {
        id: "ws".into(),
        root: directory.to_str().unwrap().into(),
    };
    let context = ToolContext {
        session_id: "session_test",
        task_id: "task",
        workspace: &workspace,
    };
    let name = "资料.bin";
    fs::File::create(directory.join(name))
        .unwrap()
        .set_len(READ_LIMIT + 1)
        .unwrap();
    let query = |path: &str| ToolCall {
        id: "stat".into(),
        name: "workspace_stat".into(),
        arguments: json!({"path":path}),
    };
    let stat = stat::Stat;
    let token = CancellationToken::new();
    let mut kernel = Kernel::in_memory(Box::new(StatProvider)).unwrap();
    register(&mut kernel).unwrap();
    let workspace = kernel.open_workspace(directory.to_str().unwrap()).unwrap();
    let session = kernel.create_session(&workspace.id).unwrap();
    let waiting = kernel.start_task(&session.id, "inspect metadata").unwrap();
    let approval = waiting.pending_approval.as_ref().unwrap();
    assert_eq!(approval.call.name, "workspace_stat");
    let done = kernel
        .respond_approval(
            &waiting.id,
            &approval.id,
            fliuno_protocol::ApprovalDecision::AllowOnce,
        )
        .unwrap();
    let output = done
        .messages
        .iter()
        .find_map(|message| match message {
            fliuno_protocol::Message::ToolResult { output, .. } => Some(output),
            _ => None,
        })
        .unwrap();
    assert_eq!(output["path"], name);
    assert_eq!(output["kind"], "file");
    assert_eq!(output["bytes"], READ_LIMIT + 1);
    assert!(output["modifiedUnixMs"].is_u64());
    assert!(output.get("content").is_none());
    assert!(kernel.list_artifacts(&done.id).unwrap().is_empty());
    let directory_result = stat.execute(&context, &query("."), &token).unwrap();
    assert_eq!(directory_result.output["kind"], "directory");
    assert!(directory_result.output["bytes"].is_null());
    assert!(
        stat.execute(&context, &query("../outside"), &token)
            .is_err()
    );
    assert!(stat.execute(&context, &query("missing"), &token).is_err());
    assert_eq!(
        directory.canonicalize().unwrap().parent(),
        Some(root.as_path())
    );
    fs::remove_dir_all(directory).unwrap();
}
