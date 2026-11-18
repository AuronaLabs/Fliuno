use super::*;
use fliuno_core::{CancellationToken, Kernel, ModelDecision, ModelProvider, ModelTurn, ToolSpec};
use fliuno_protocol::{ApprovalDecision, EventKind, Message, Task, TaskStatus, ToolCall};
use serde_json::json;
use std::{fs, path::PathBuf};

struct Directory(PathBuf);
impl Directory {
    fn new() -> Self {
        let root =
            std::env::temp_dir().join(format!("fliuno-skill-resources-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(root.join("skills/example/references")).unwrap();
        fs::create_dir_all(root.join("skills/example/scripts")).unwrap();
        fs::create_dir_all(root.join("skills/other")).unwrap();
        for name in ["example", "other"] {
            fs::write(root.join("skills").join(name).join("SKILL.md"), format!("---\nname: {name}\ndescription: Resource fixture\nallowed-tools: shell\n---\nRead references when needed.\n")).unwrap();
        }
        fs::write(
            root.join("skills/example/references/报告.md"),
            "你好，参考资料。\n",
        )
        .unwrap();
        fs::write(
            root.join("skills/example/scripts/do-not-run.py"),
            "raise RuntimeError('must remain text')\n",
        )
        .unwrap();
        fs::write(
            root.join("skills/other/private.txt"),
            "other skill's private content",
        )
        .unwrap();
        Self(root)
    }
    fn catalog(&self) -> SkillCatalog {
        SkillCatalog::open(self.0.join("skills")).unwrap()
    }
}
impl Drop for Directory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn resources_are_progressive_sorted_bounded_utf8_and_hashed_without_executing_scripts() {
    let fixture = Directory::new();
    let catalog = fixture.catalog();
    let listing = catalog.list_resources("example", "").unwrap();
    assert_eq!(
        listing
            .iter()
            .map(|entry| entry.name.as_str())
            .collect::<Vec<_>>(),
        vec!["SKILL.md", "references", "scripts"]
    );
    let resource = catalog
        .read_resource("example", "references/报告.md")
        .unwrap();
    assert_eq!(resource.content, "你好，参考资料。\n");
    assert_eq!(resource.bytes, resource.content.len());
    assert_eq!(
        resource.sha256,
        format!("{:x}", Sha256::digest(resource.content.as_bytes()))
    );
    assert!(
        catalog
            .read_resource("example", "scripts/do-not-run.py")
            .unwrap()
            .content
            .contains("must remain text")
    );
    fs::write(
        fixture.0.join("skills/example/references/binary.dat"),
        [0xff, 0xfe],
    )
    .unwrap();
    assert!(
        catalog
            .read_resource("example", "references/binary.dat")
            .unwrap_err()
            .contains("UTF-8")
    );
    let large = fs::File::create(fixture.0.join("skills/example/references/large.txt")).unwrap();
    large.set_len(MAX_RESOURCE_BYTES + 1).unwrap();
    assert!(
        catalog
            .read_resource("example", "references/large.txt")
            .unwrap_err()
            .contains("256 KiB")
    );
    assert!(catalog.read_resource("unknown", "SKILL.md").is_err());
    assert!(catalog.read_resource("example", "references").is_err());
}

#[test]
fn unsafe_paths_and_directory_scans_are_rejected_without_reading_sibling_skills() {
    let fixture = Directory::new();
    let catalog = fixture.catalog();
    for path in [
        "",
        "../other/private.txt",
        "references/../../other/private.txt",
        "/absolute",
        "C:/host",
        "references\\报告.md",
        "references//x",
        "references/.",
        "file:stream",
        "NUL.txt",
        "COM1",
        "LPT¹.txt",
        "trailing.",
    ] {
        assert!(
            catalog.read_resource("example", path).is_err(),
            "accepted {path:?}"
        );
    }
    let many = fixture.0.join("skills/example/many");
    fs::create_dir(&many).unwrap();
    for index in 0..257 {
        fs::write(many.join(format!("{index}.txt")), "small").unwrap();
    }
    assert!(
        catalog
            .list_resources("example", "many")
            .unwrap_err()
            .contains("256 entries")
    );
}

#[test]
fn directory_links_cannot_escape_a_skill_even_to_a_sibling_inside_the_catalog() {
    let fixture = Directory::new();
    let catalog = fixture.catalog();
    let link = fixture.0.join("skills/example/references/redirect");
    let target = fixture.0.join("skills/other");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&target, &link).unwrap();
    #[cfg(windows)]
    {
        let output = std::process::Command::new("cmd.exe")
            .args(["/D", "/C", "mklink", "/J"])
            .arg(link.to_string_lossy().replace('/', "\\"))
            .arg(target.to_string_lossy().replace('/', "\\"))
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    assert!(
        catalog
            .read_resource("example", "references/redirect/private.txt")
            .is_err()
    );
    assert!(
        catalog
            .read_asset("example", "references/redirect/private.txt")
            .is_err()
    );
    assert!(
        catalog
            .list_resources("example", "references/redirect")
            .is_err()
    );
    assert_eq!(
        fs::read_to_string(target.join("private.txt")).unwrap(),
        "other skill's private content"
    );
    // Explicitly unlink only our link. Never recursively remove its target.
    #[cfg(windows)]
    fs::remove_dir(link).unwrap();
    #[cfg(unix)]
    fs::remove_file(link).unwrap();
}

#[cfg(unix)]
#[test]
fn file_links_and_fifos_are_rejected_without_blocking_the_reader() {
    use std::os::unix::fs::symlink;
    let fixture = Directory::new();
    let catalog = fixture.catalog();
    let directory = fixture.0.join("skills/example/references");
    symlink(
        fixture.0.join("skills/other/private.txt"),
        directory.join("link.txt"),
    )
    .unwrap();
    assert!(
        catalog
            .read_resource("example", "references/link.txt")
            .is_err()
    );
    let status = std::process::Command::new("mkfifo")
        .arg(directory.join("fifo"))
        .status()
        .unwrap();
    assert!(status.success());
    assert!(catalog.read_resource("example", "references/fifo").is_err());

    // A replaced instruction file must obey the same final-component rule,
    // both for an existing catalog and during a fresh discovery.
    let instructions = fixture.0.join("skills/example/SKILL.md");
    fs::remove_file(&instructions).unwrap();
    symlink(fixture.0.join("skills/other/SKILL.md"), &instructions).unwrap();
    assert!(catalog.read("example").is_err());
    assert!(SkillCatalog::open(fixture.0.join("skills")).is_err());
}

struct Provider;
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
            ModelDecision::Complete("resource observed".into())
        } else {
            ModelDecision::Call(ToolCall {
                id: format!("read_{}", task.id),
                name: "skills_read_resource".into(),
                arguments: json!({"name":"example","path":"references/报告.md"}),
            })
        }
        .into())
    }
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
fn a_conflict_with_the_last_resource_tool_does_not_partially_register_skills() {
    let fixture = Directory::new();
    let mut kernel = Kernel::open(fixture.0.join("kernel.db"), Box::new(Provider)).unwrap();
    let conflict = tools(std::sync::Arc::new(fixture.catalog())).pop().unwrap();
    let original = conflict.spec().name;
    assert_eq!(original, "skills_read_resource");
    kernel.register_tool(conflict).unwrap();
    let before: Vec<_> = kernel
        .tool_specs()
        .into_iter()
        .map(|spec| spec.name)
        .collect();
    assert!(crate::register(&mut kernel, fixture.catalog()).is_err());
    assert_eq!(
        kernel
            .tool_specs()
            .into_iter()
            .map(|spec| spec.name)
            .collect::<Vec<_>>(),
        before
    );
}

#[test]
fn changed_catalog_source_cannot_rebind_persisted_resource_consent() {
    let first = Directory::new();
    let second = Directory::new();
    let database = first.0.join("kernel.db");
    let mut kernel = Kernel::open(&database, Box::new(Provider)).unwrap();
    crate::register(&mut kernel, first.catalog()).unwrap();
    let workspace = kernel.open_workspace(first.0.to_str().unwrap()).unwrap();
    let session = kernel.create_session(&workspace.id).unwrap();
    let original = kernel
        .start_task(&session.id, "read original source")
        .unwrap();
    assert!(original.pending_approval.is_some());
    assert!(
        !serde_json::to_string(&original)
            .unwrap()
            .contains("fliuno-skill-resources-")
    );
    drop(kernel);
    let mut kernel = Kernel::open(&database, Box::new(Provider)).unwrap();
    crate::register(&mut kernel, second.catalog()).unwrap();
    assert_eq!(approve(&mut kernel, &original).status, TaskStatus::Failed);
    assert!(
        !kernel
            .list_events(&original.id, 0, 100)
            .unwrap()
            .events
            .iter()
            .any(|event| matches!(event.data, EventKind::ToolStarted { .. }))
    );
    let fresh = kernel.start_task(&session.id, "read new source").unwrap();
    let done = approve(&mut kernel, &fresh);
    assert_eq!(done.status, TaskStatus::Completed);
    let output = done
        .messages
        .iter()
        .find_map(|message| match message {
            Message::ToolResult { output, .. } => Some(output),
            _ => None,
        })
        .unwrap();
    assert_eq!(output["content"], "你好，参考资料。\n");
    assert_eq!(output["grantsPermissions"], false);
    drop(kernel);
    let kernel = Kernel::open(&database, Box::new(Provider)).unwrap();
    assert_eq!(
        kernel.get_task(&original.id).unwrap().status,
        TaskStatus::Failed
    );
}

#[test]
fn each_resource_tool_rejects_authority_overrides_and_cancellation_before_io() {
    let fixture = Directory::new();
    let tools = tools(std::sync::Arc::new(fixture.catalog()));
    let workspace = fliuno_protocol::Workspace {
        id: "w".into(),
        root: fixture.0.to_string_lossy().into_owned(),
    };
    let context = fliuno_core::ToolContext {
        task_id: "t",
        session_id: "s",
        workspace: &workspace,
    };
    for tool in tools {
        for encoding in [json!(null), json!("hex"), json!(123)] {
            assert!(
                tool.validate(&json!({"name":"example","path":"SKILL.md","encoding":encoding}))
                    .is_err()
            );
        }
        if tool.spec().name == "skills_list_resources" {
            assert!(
                tool.validate(&json!({"name":"example","encoding":"base64"}))
                    .is_err()
            );
        }
        assert!(
            tool.validate(&json!({"name":"example","path":"references","command":"python"}))
                .is_err()
        );
        assert!(
            tool.validate(&json!({"name":"example","path":"../other"}))
                .is_err()
        );
        let cancellation = CancellationToken::new();
        cancellation.cancel();
        let call = ToolCall {
            id: "c".into(),
            name: tool.spec().name,
            arguments: json!({"name":"example","path":"references/报告.md"}),
        };
        assert!(tool.execute(&context, &call, &cancellation).is_err());
    }
}

#[test]
fn opaque_assets_preserve_raw_bytes_hash_and_exact_budget_instead_of_decoding() {
    let fixture = Directory::new();
    let catalog = fixture.catalog();
    let path = fixture.0.join("skills/example/references/opaque.bin");
    let bytes: Vec<u8> = (0..=255)
        .cycle()
        .take(MAX_RESOURCE_BYTES as usize)
        .collect();
    fs::write(&path, &bytes).unwrap();
    assert!(
        catalog
            .read_resource("example", "references/opaque.bin")
            .is_err()
    );
    let asset = catalog
        .read_asset("example", "references/opaque.bin")
        .unwrap();
    assert_eq!(asset.encoding, "base64");
    assert_eq!(asset.bytes, bytes.len());
    assert_eq!(STANDARD.decode(&asset.data).unwrap(), bytes);
    assert_eq!(asset.sha256, format!("{:x}", Sha256::digest(&bytes)));
    assert!(serde_json::to_vec(&asset).unwrap().len() < 1024 * 1024);
    fs::OpenOptions::new()
        .write(true)
        .open(&path)
        .unwrap()
        .set_len(MAX_RESOURCE_BYTES + 1)
        .unwrap();
    assert!(
        catalog
            .read_asset("example", "references/opaque.bin")
            .unwrap_err()
            .contains("256 KiB")
    );
    for invalid in [
        "../other/private.txt",
        "references",
        "references/../SKILL.md",
    ] {
        assert!(catalog.read_asset("example", invalid).is_err());
    }
    fs::write(&path, []).unwrap();
    let empty = catalog
        .read_asset("example", "references/opaque.bin")
        .unwrap();
    assert_eq!(empty.data, "");
    assert_eq!(empty.bytes, 0);
}

struct BinaryProvider;
impl ModelProvider for BinaryProvider {
    fn next(
        &self,
        task: &Task,
        tools: &[ToolSpec],
        cancellation: &CancellationToken,
    ) -> Result<ModelTurn, String> {
        let mut turn = Provider.next(task, tools, cancellation)?;
        if let ModelDecision::Call(call) = &mut turn.decision {
            call.arguments =
                json!({"name":"example","path":"references/opaque.bin","encoding":"base64"});
        }
        Ok(turn)
    }
}

#[test]
fn binary_assets_require_consent_and_persist_as_tool_data_without_creating_artifacts() {
    let fixture = Directory::new();
    let bytes = [0u8, 255, 13, 10, 128];
    fs::write(
        fixture.0.join("skills/example/references/opaque.bin"),
        bytes,
    )
    .unwrap();
    let database = fixture.0.join("kernel.db");
    let mut kernel = Kernel::open(&database, Box::new(BinaryProvider)).unwrap();
    crate::register(&mut kernel, fixture.catalog()).unwrap();
    let workspace = kernel.open_workspace(fixture.0.to_str().unwrap()).unwrap();
    let session = kernel.create_session(&workspace.id).unwrap();
    let denied = kernel.start_task(&session.id, "deny binary").unwrap();
    let denied = kernel
        .respond_approval(
            &denied.id,
            &denied.pending_approval.as_ref().unwrap().id,
            ApprovalDecision::Deny,
        )
        .unwrap();
    assert_eq!(denied.status, TaskStatus::Failed);
    assert!(
        !denied
            .messages
            .iter()
            .any(|message| matches!(message, Message::ToolResult { .. }))
    );
    let waiting = kernel.start_task(&session.id, "read binary").unwrap();
    assert_eq!(
        waiting.pending_approval.as_ref().unwrap().call.arguments["encoding"],
        "base64"
    );
    drop(kernel);
    let mut kernel = Kernel::open(&database, Box::new(BinaryProvider)).unwrap();
    crate::register(&mut kernel, fixture.catalog()).unwrap();
    let done = approve(&mut kernel, &waiting);
    assert_eq!(done.status, TaskStatus::Completed);
    let output = done
        .messages
        .iter()
        .find_map(|message| match message {
            Message::ToolResult { output, .. } => Some(output),
            _ => None,
        })
        .unwrap();
    assert_eq!(
        STANDARD.decode(output["data"].as_str().unwrap()).unwrap(),
        bytes
    );
    assert_eq!(output["bytes"], bytes.len());
    assert_eq!(output["encoding"], "base64");
    assert_eq!(output["grantsPermissions"], false);
    assert!(output.get("content").is_none());
    assert!(
        !kernel
            .list_events(&done.id, 0, 100)
            .unwrap()
            .events
            .iter()
            .any(|event| matches!(event.data, EventKind::ArtifactCreated { .. }))
    );
    let expected = serde_json::to_value(&done).unwrap();
    drop(kernel);
    let kernel = Kernel::open(&database, Box::new(BinaryProvider)).unwrap();
    assert_eq!(
        serde_json::to_value(kernel.get_task(&done.id).unwrap()).unwrap(),
        expected
    );
}
