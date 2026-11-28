//! Exact edits preserve bytes outside a unique, reviewed match.
use super::*;

pub(crate) const MAX_EDIT_TEXT_BYTES: usize = 16 * 1024;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct EditArgs {
    path: String,
    base_sha256: String,
    old_text: String,
    new_text: String,
}

pub(crate) fn validate(arguments: &Value) -> Result<(), String> {
    parse(arguments).map(|_| ())
}

fn parse(arguments: &Value) -> Result<EditArgs, String> {
    let args: EditArgs = serde_json::from_value(arguments.clone())
        .map_err(|_| "expected path, baseSha256, oldText and newText")?;
    relative(&args.path, false)?;
    if args.base_sha256.len() != 64
        || !args
            .base_sha256
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
    {
        return Err("baseSha256 must be a lowercase SHA-256 hex digest".into());
    }
    if args.old_text.is_empty()
        || [args.old_text.len(), args.new_text.len()]
            .iter()
            .any(|&len| len > MAX_EDIT_TEXT_BYTES)
    {
        return Err("oldText must not be empty and each text field must be at most 16 KiB".into());
    }
    if args.old_text == args.new_text {
        return Err("oldText and newText must differ".into());
    }
    Ok(args)
}

pub(crate) fn execute(
    directory: &Dir,
    arguments: &Value,
    cancellation: &CancellationToken,
) -> Result<Value, ToolError> {
    let args = parse(arguments)?;
    let mut file = workspace::open_target(directory, &args.path, true)
        .map_err(|_| "cannot open workspace target for editing")?;
    let mut current = workspace::read_snapshot(&mut file, cancellation)?;
    if sha256(current.as_bytes()) != args.base_sha256 {
        return Err(ToolError::NeedsCorrection {
            code: "workspace_base_changed",
            message:
                "No changes written. The file changed; reread it before proposing a fresh edit."
                    .into(),
        });
    }
    let start = current.find(&args.old_text).ok_or_else(|| ToolError::NeedsCorrection {
        code: "workspace_edit_not_found",
        message: "No changes written. Use exact text copied from the file, including its whitespace and line endings.".into(),
    })?;
    // Search from the next character so overlapping occurrences are ambiguous too.
    let next = start
        + args
            .old_text
            .chars()
            .next()
            .expect("nonempty oldText")
            .len_utf8();
    if current[next..].contains(&args.old_text) {
        return Err(ToolError::NeedsCorrection {
            code: "workspace_edit_ambiguous",
            message:
                "No changes written. Add surrounding text so that oldText matches exactly once."
                    .into(),
        });
    }
    let len = current.len() - args.old_text.len() + args.new_text.len();
    if len > READ_LIMIT as usize {
        return Err("edited file exceeds 256 KiB".into());
    }
    if cancellation.is_cancelled() {
        return Err("tool cancelled before file update".into());
    }
    current.replace_range(start..start + args.old_text.len(), &args.new_text);
    file.seek(SeekFrom::Start(0))
        .map_err(|_| "cannot seek workspace target")?;
    file.write_all(current.as_bytes()).map_err(|_| {
        ToolError::OutcomeUnknown("workspace edit write failed; inspect target".into())
    })?;
    file.set_len(len as u64).map_err(|_| {
        ToolError::OutcomeUnknown("workspace edit truncate failed; inspect target".into())
    })?;
    file.sync_all().map_err(|_| {
        ToolError::OutcomeUnknown("workspace edit sync failed; inspect target".into())
    })?;
    Ok(json!({"path":args.path,"sha256":sha256(current.as_bytes()),"bytesWritten":len}))
}

#[cfg(test)]
mod tests {
    use super::*;
    use fliuno_core::{ModelDecision, ModelProvider, ModelTurn};
    use fliuno_protocol::{ApprovalDecision, Message, Task, TaskStatus};
    use std::{
        fs,
        time::{SystemTime, UNIX_EPOCH},
    };

    const BEFORE: &str = "保留\r\n  ααα\r\nlast\n";
    const AFTER: &str = "保留\r\n  beta\nlast\n";

    struct EditFixture;
    impl ModelProvider for EditFixture {
        fn next(
            &self,
            task: &Task,
            _: &[ToolSpec],
            _: &CancellationToken,
        ) -> Result<ModelTurn, String> {
            let last_result = task
                .messages
                .iter()
                .rev()
                .find_map(|message| match message {
                    Message::ToolResult { output, .. } => Some(output),
                    _ => None,
                });
            let old = match task.model_steps {
                1 => "\tααα",
                2 => {
                    assert_eq!(last_result.unwrap()["code"], "workspace_edit_not_found");
                    "αα"
                }
                3 => {
                    assert_eq!(last_result.unwrap()["code"], "workspace_edit_ambiguous");
                    "ααα\r\n"
                }
                _ => {
                    if last_result.unwrap()["code"] == "workspace_base_changed" {
                        return Ok(ModelDecision::Complete("base changed".into()).into());
                    }
                    assert_eq!(last_result.unwrap()["sha256"], sha256(AFTER.as_bytes()));
                    return Ok(ModelDecision::Complete("edited".into()).into());
                }
            };
            Ok(ModelDecision::Call(ToolCall {
                id: format!("edit-{}", task.model_steps),
                name: "workspace_edit_text".into(),
                arguments: json!({"path":"source.txt","baseSha256":sha256(BEFORE.as_bytes()), "oldText":old, "newText":"beta\n"}),
            }).into())
        }
    }

    #[test]
    fn exact_edit_recovers_from_missing_and_overlapping_matches_and_checks_the_reviewed_base() {
        let root = std::env::temp_dir().join(format!(
            "fliuno-edit-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&root).unwrap();
        let file = root.join("source.txt");
        let mut kernel = Kernel::in_memory(Box::new(EditFixture)).unwrap();
        register(&mut kernel).unwrap();
        let workspace = kernel.open_workspace(root.to_str().unwrap()).unwrap();
        for changed in [false, true] {
            fs::write(&file, BEFORE).unwrap();
            let session = kernel.create_session(&workspace.id).unwrap();
            let mut task = kernel.start_task(&session.id, "edit the file").unwrap();
            let mut approvals = Vec::new();
            for step in 1..=3 {
                assert_eq!(task.status, TaskStatus::WaitingApproval);
                assert_eq!(fs::read(&file).unwrap(), BEFORE.as_bytes());
                let approval = task.pending_approval.as_ref().unwrap();
                assert!(!approvals.contains(&approval.id));
                approvals.push(approval.id.clone());
                if step == 3 && changed {
                    fs::write(&file, "user changes\r\n").unwrap();
                }
                task = kernel
                    .respond_approval(&task.id, &approval.id, ApprovalDecision::AllowOnce)
                    .unwrap();
            }
            assert_eq!(task.status, TaskStatus::Completed);
            let (output, bytes) = if changed {
                ("base changed", "user changes\r\n")
            } else {
                ("edited", AFTER)
            };
            assert_eq!(task.output.as_deref(), Some(output));
            assert_eq!(fs::read(&file).unwrap(), bytes.as_bytes());
        }
        fs::remove_dir_all(root).unwrap();
    }
}
