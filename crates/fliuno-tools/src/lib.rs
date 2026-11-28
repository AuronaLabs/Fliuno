//! Built-in workspace tools. Directory capabilities enforce the root during IO;
//! lexical checks alone are insufficient against symlinks and path races.
mod batch_paths;
mod directory_page;
mod edit;
mod file_io;
mod find;
mod hash;
mod import_artifact;
mod search;
mod stat;
mod workspace;
use base64::{Engine, engine::general_purpose::STANDARD};
use cap_fs_ext::{FollowSymlinks, MetadataExt, OpenOptionsFollowExt};
use cap_std::{
    ambient_authority,
    fs::{Dir, OpenOptions},
};
use diffy::{
    apply,
    patch_set::{FileOperation, ParseOptions, PatchSet},
};
use fliuno_core::{
    CancellationToken, Kernel, Tool, ToolContext, ToolEffect, ToolError, ToolOutput,
    ToolProposalError, ToolSpec,
};
use fliuno_protocol::ToolCall;
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    io::{Seek, SeekFrom, Write},
    path::{Component, Path},
    sync::{Arc, Mutex},
};
pub use workspace::{WriteProposal, prepare_workspace_write};

const READ_LIMIT: u64 = 256 * 1024;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FileArgs {
    path: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadArgs {
    path: String,
    line: Option<u32>,
    limit: Option<u32>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WriteArgs {
    path: String,
    content: String,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PatchArgs {
    path: String,
    base_sha256: String,
    patch: String,
}

#[derive(Clone, Copy)]
enum Operation {
    List,
    Read,
    ReadBinary,
    Create,
    Patch,
    Edit,
    Replace,
}
struct WorkspaceTool(Operation, Arc<Mutex<()>>);

impl WorkspaceTool {
    fn new(operation: Operation, write_lock: Arc<Mutex<()>>) -> Self {
        Self(operation, write_lock)
    }
}

pub fn register(kernel: &mut Kernel) -> fliuno_core::Result<()> {
    let write_lock = Arc::new(Mutex::new(()));
    let mut tools: Vec<Box<dyn Tool>> = [
        Operation::List,
        Operation::Read,
        Operation::ReadBinary,
        Operation::Create,
        Operation::Patch,
        Operation::Edit,
        Operation::Replace,
    ]
    .into_iter()
    .map(|operation| {
        Box::new(WorkspaceTool::new(operation, Arc::clone(&write_lock))) as Box<dyn Tool>
    })
    .collect();
    tools.push(Box::new(directory_page::ListPage));
    tools.push(Box::new(stat::Stat));
    tools.push(Box::new(stat::StatBatch));
    tools.push(Box::new(hash::Hash));
    tools.push(Box::new(hash::HashBatch));
    tools.push(Box::new(import_artifact::ImportArtifact));
    tools.push(Box::new(search::Search));
    tools.push(Box::new(find::Find));
    kernel.register_tools(tools)
}

fn relative(path: &str, allow_root: bool) -> Result<&Path, String> {
    let parsed = Path::new(path);
    if allow_root && path == "." {
        return Ok(parsed);
    }
    // Reject Windows ADS, drive/UNC paths and traversal on every host, including
    // when a Linux CI job validates a path originally supplied on Windows.
    if path.chars().any(char::is_control)
        || path.split('/').any(|part| {
            let base = part.split('.').next().unwrap_or("").to_ascii_uppercase();
            part.ends_with([' ', '.'])
                || matches!(base.as_str(), "CON" | "PRN" | "AUX" | "NUL")
                || (base.len() == 4
                    && (base.starts_with("COM") || base.starts_with("LPT"))
                    && matches!(base.as_bytes()[3], b'1'..=b'9'))
        })
        || path.is_empty()
        || path.len() > 4096
        || path.contains([':', '\\', '\0'])
        || path
            .split('/')
            .any(|p| p.is_empty() || p == "." || p == "..")
        || !parsed
            .components()
            .all(|c| matches!(c, Component::Normal(_)))
    {
        return Err(
            "path must be a relative forward-slash path without traversal or drive/ADS syntax"
                .into(),
        );
    }
    Ok(parsed)
}

fn validate_patch(args: &PatchArgs) -> Result<(), ToolProposalError> {
    relative(&args.path, false)?;
    if args.base_sha256.len() != 64
        || !args
            .base_sha256
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return Err(ToolProposalError::Rejected(
            "baseSha256 must be a lowercase SHA-256 hex digest".into(),
        ));
    }
    if args.patch.is_empty() || args.patch.len() > 65_536 {
        return Err(ToolProposalError::Rejected(
            "patch must contain 1..65536 UTF-8 bytes".into(),
        ));
    }
    let mut patches = PatchSet::parse(&args.patch, ParseOptions::unidiff());
    let invalid_format = || {
        ToolProposalError::Correctable {
        code: "workspace_patch_format",
        message: "Invalid unified diff. Supply one text diff with --- a/<path>, +++ b/<path>, valid @@ line ranges, and matching hunk line counts. No file was modified.".into(),
    }
    };
    let file_patch = patches
        .next()
        .ok_or_else(invalid_format)?
        .map_err(|_| invalid_format())?;
    if patches.next().is_some() {
        return Err(ToolProposalError::Rejected(
            "patch must modify exactly one file".into(),
        ));
    }
    match file_patch.operation().strip_prefix(1) {
        FileOperation::Modify { original, modified }
            if original == args.path && modified == args.path => {}
        _ => {
            return Err(ToolProposalError::Rejected(
                "diff paths must both identify the requested file".into(),
            ));
        }
    }
    if file_patch.patch().as_text().is_none() {
        return Err(ToolProposalError::Rejected(
            "only text patches are supported".into(),
        ));
    }
    Ok(())
}

fn sha256(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

impl Tool for WorkspaceTool {
    fn supports_read_rules(&self) -> bool {
        matches!(self.0, Operation::List | Operation::Read)
    }

    fn spec(&self) -> ToolSpec {
        let (name, description, effect) = match self.0 {
            Operation::List => (
                "workspace_list",
                "List up to 256 entries inside the current workspace. Use path '.' for its root.",
                ToolEffect::WorkspaceRead,
            ),
            Operation::Read => (
                "workspace_read_text",
                "Read a UTF-8 file (maximum 256 KiB) inside the current workspace.",
                ToolEffect::WorkspaceRead,
            ),
            Operation::ReadBinary => (
                "workspace_read_bytes",
                "Read opaque file bytes as base64 (maximum 256 KiB) inside the current workspace. This does not decode or execute the file.",
                ToolEffect::WorkspaceRead,
            ),
            Operation::Create => (
                "workspace_create_text",
                "Create a NEW UTF-8 file inside the current workspace after approval. Never overwrites an existing path.",
                ToolEffect::WorkspaceWrite,
            ),
            Operation::Patch => (
                "workspace_patch_text",
                "Review and apply one single-file unified diff to an existing UTF-8 workspace file after approval. Read it first, use the returned sha256 as baseSha256, and use --- a/<path> / +++ b/<path> headers.",
                ToolEffect::WorkspaceWrite,
            ),
            Operation::Edit => (
                "workspace_edit_text",
                "Replace one exact occurrence of oldText with newText in an existing UTF-8 file after approval. Read it first and reuse its SHA-256. Include surrounding text to make the match unique. Preserve the original whitespace and line endings in oldText.",
                ToolEffect::WorkspaceWrite,
            ),
            Operation::Replace => (
                "workspace_replace_text",
                "Create or replace a UTF-8 workspace file after review. Existing files require the exact baseSha256 from a prior read; null only creates a new file.",
                ToolEffect::WorkspaceWrite,
            ),
        };
        let mut schema = json!({"type":"object","required":["path"],"properties":{"path":{"type":"string"}},"additionalProperties":false});
        match self.0 {
            Operation::Create => {
                schema["required"] = json!(["path", "content"]);
                schema["properties"]["content"] = json!({"type":"string","maxLength":262144});
            }
            Operation::Patch => {
                schema["required"] = json!(["path", "baseSha256", "patch"]);
                schema["properties"]["baseSha256"] =
                    json!({"type":"string","pattern":"^[0-9a-f]{64}$"});
                schema["properties"]["patch"] =
                    json!({"type":"string","minLength":1,"maxLength":65536});
            }
            Operation::Edit => {
                schema["required"] = json!(["path", "baseSha256", "oldText", "newText"]);
                schema["properties"]["baseSha256"] =
                    json!({"type":"string","pattern":"^[0-9a-f]{64}$"});
                schema["properties"]["oldText"] =
                    json!({"type":"string","minLength":1,"maxLength":edit::MAX_EDIT_TEXT_BYTES});
                schema["properties"]["newText"] =
                    json!({"type":"string","maxLength":edit::MAX_EDIT_TEXT_BYTES});
            }
            Operation::Replace => {
                schema["required"] = json!(["path", "content", "baseSha256"]);
                schema["properties"]["content"] = json!({"type":"string","maxLength":262144});
                schema["properties"]["baseSha256"] =
                    json!({"type":["string","null"],"pattern":"^[0-9a-f]{64}$"});
            }
            Operation::Read => {
                schema["properties"]["line"] =
                    json!({"type":"integer","minimum":1,"maximum":4294967295_u64});
                schema["properties"]["limit"] =
                    json!({"type":"integer","minimum":1,"maximum":4294967295_u64});
            }
            Operation::ReadBinary => {}
            Operation::List => {}
        }
        ToolSpec {
            name: name.into(),
            description: description.into(),
            effect,
            input_schema: schema,
        }
    }

    fn validate(&self, arguments: &Value) -> Result<(), String> {
        match self.0 {
            Operation::Create => {
                let args: WriteArgs = serde_json::from_value(arguments.clone())
                    .map_err(|_| "expected path and content strings")?;
                relative(&args.path, false)?;
                if args.content.len() > READ_LIMIT as usize {
                    return Err("content exceeds 256 KiB".into());
                }
            }
            Operation::Patch => {
                let args: PatchArgs = serde_json::from_value(arguments.clone())
                    .map_err(|_| "expected path, baseSha256 and patch")?;
                validate_patch(&args).map_err(|error| error.to_string())?;
            }
            Operation::Edit => {
                edit::validate(arguments)?;
            }
            Operation::Replace => {
                workspace::validate_replace(arguments)?;
            }
            Operation::Read => {
                let args: ReadArgs = serde_json::from_value(arguments.clone())
                    .map_err(|_| "expected path with optional line and limit")?;
                relative(&args.path, false)?;
                if args.line == Some(0) || args.limit == Some(0) {
                    return Err("line and limit must be positive".into());
                }
            }
            Operation::ReadBinary => {
                let args: FileArgs = serde_json::from_value(arguments.clone())
                    .map_err(|_| "expected a path string")?;
                relative(&args.path, false)?;
            }
            Operation::List => {
                let args: FileArgs = serde_json::from_value(arguments.clone())
                    .map_err(|_| "expected a path string")?;
                relative(&args.path, matches!(self.0, Operation::List))?;
            }
        }
        Ok(())
    }

    fn validate_model_proposal(&self, arguments: &Value) -> Result<(), ToolProposalError> {
        if matches!(self.0, Operation::Patch) {
            let args: PatchArgs = serde_json::from_value(arguments.clone()).map_err(|_| {
                ToolProposalError::Rejected("expected path, baseSha256 and patch".into())
            })?;
            validate_patch(&args)
        } else {
            self.validate(arguments)
                .map_err(ToolProposalError::Rejected)
        }
    }

    fn execute(
        &self,
        context: &ToolContext<'_>,
        call: &ToolCall,
        cancellation: &CancellationToken,
    ) -> Result<ToolOutput, ToolError> {
        if cancellation.is_cancelled() {
            return Err(ToolError::Failed("tool cancelled before execution".into()));
        }
        self.validate(&call.arguments)?;
        let _write_guard = if matches!(
            self.0,
            Operation::Create | Operation::Patch | Operation::Edit | Operation::Replace
        ) {
            Some(
                self.1
                    .lock()
                    .map_err(|_| ToolError::Failed("workspace write lock is poisoned".into()))?,
            )
        } else {
            None
        };
        if cancellation.is_cancelled() {
            return Err("tool cancelled before workspace access".into());
        }
        let directory = Dir::open_ambient_dir(&context.workspace.root, ambient_authority())
            .map_err(|_| "cannot open workspace capability")?;
        let output = match self.0 {
            Operation::List => {
                let args: FileArgs = serde_json::from_value(call.arguments.clone())
                    .map_err(|_| "invalid list arguments")?;
                let mut entries = Vec::new();
                let mut truncated = false;
                for entry in directory
                    .read_dir(&args.path)
                    .map_err(|_| "cannot list workspace path")?
                {
                    if cancellation.is_cancelled() {
                        return Err(ToolError::Failed("tool cancelled".into()));
                    }
                    let entry = entry.map_err(|_| "cannot read directory entry")?;
                    if entries.len() == 256 {
                        truncated = true;
                        break;
                    }
                    let kind = entry.file_type().map_err(|_| "cannot read entry type")?;
                    entries.push(json!({"name":entry.file_name().to_string_lossy(),"kind":if kind.is_symlink(){"symlink"}else if kind.is_dir(){"directory"}else{"file"}}));
                }
                entries.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
                json!({"entries":entries,"truncated":truncated})
            }
            Operation::Read => {
                let args: ReadArgs = serde_json::from_value(call.arguments.clone())
                    .map_err(|_| "invalid read arguments")?;
                let mut file = directory
                    .open_with(&args.path, &file_io::options(false))
                    .map_err(|_| "cannot open workspace file")?;
                let bytes = file_io::read(&mut file, cancellation)?;
                let text = String::from_utf8(bytes).map_err(|_| "file is not UTF-8 text")?;
                let digest = sha256(text.as_bytes());
                let text = if args.line.is_some() || args.limit.is_some() {
                    text.split_inclusive('\n')
                        .skip(args.line.unwrap_or(1) as usize - 1)
                        .take(args.limit.map_or(usize::MAX, |n| n as usize))
                        .collect::<String>()
                } else {
                    text
                };
                json!({"path":args.path,"content":text,"sha256":digest})
            }
            Operation::ReadBinary => {
                let args: FileArgs = serde_json::from_value(call.arguments.clone())
                    .map_err(|_| "invalid binary read arguments")?;
                let mut file = directory
                    .open_with(&args.path, &file_io::options(false))
                    .map_err(|_| "cannot open workspace file")?;
                let bytes = file_io::read(&mut file, cancellation)?;
                let digest = sha256(&bytes);
                json!({"path":args.path,"encoding":"base64","data":STANDARD.encode(&bytes),
                    "bytes":bytes.len(),"sha256":digest,"grantsPermissions":false})
            }
            Operation::Create => {
                if cancellation.is_cancelled() {
                    return Err(ToolError::Failed("tool cancelled before write".into()));
                }
                let args: WriteArgs = serde_json::from_value(call.arguments.clone())
                    .map_err(|_| "invalid create arguments")?;
                // create_new is atomic with respect to existing targets. Capability
                // resolution also prevents a symlink from redirecting the parent.
                let mut file=directory.open_with(&args.path,OpenOptions::new().create_new(true).write(true)).map_err(|_|"cannot create new workspace file (parent must exist; target must not exist)")?;
                file.write_all(args.content.as_bytes()).map_err(|_| {
                    ToolError::OutcomeUnknown("workspace write failed; inspect target".into())
                })?;
                file.sync_all().map_err(|_| {
                    ToolError::OutcomeUnknown("workspace sync failed; inspect target".into())
                })?;
                json!({"path":args.path,"bytesWritten":args.content.len()})
            }
            Operation::Patch => {
                let args: PatchArgs = serde_json::from_value(call.arguments.clone())
                    .map_err(|_| "invalid patch arguments")?;
                let (parent_path, file_name) = args
                    .path
                    .rsplit_once('/')
                    .unwrap_or((".", args.path.as_str()));
                let parent = directory
                    .open_dir(parent_path)
                    .map_err(|_| "cannot open workspace file directory")?;
                let metadata = parent
                    .symlink_metadata(file_name)
                    .map_err(|_| "workspace file does not exist")?;
                if metadata.file_type().is_symlink() || !metadata.is_file() {
                    return Err("patch target must be a regular, non-symlink file".into());
                }
                if metadata.len() > READ_LIMIT {
                    return Err("file exceeds 256 KiB".into());
                }
                let mut file = parent
                    .open_with(file_name, file_io::options(true).follow(FollowSymlinks::No))
                    .map_err(|_| "cannot open workspace file for editing")?;
                let opened_metadata = file
                    .metadata()
                    .map_err(|_| "cannot read workspace file metadata")?;
                if !opened_metadata.is_file() || opened_metadata.nlink() != 1 {
                    return Err(
                        "patch target must be a regular file without additional hard links".into(),
                    );
                }
                let bytes = file_io::read(&mut file, cancellation)?;
                if sha256(&bytes) != args.base_sha256 {
                    return Err(ToolError::NeedsCorrection {
                        code: "workspace_base_changed",
                        message: "workspace file changed since proposal; reread it and prepare a new patch".into(),
                    });
                }
                let current =
                    String::from_utf8(bytes).map_err(|_| "patch target is not UTF-8 text")?;
                let mut patches = PatchSet::parse(&args.patch, ParseOptions::unidiff());
                let file_patch = patches
                    .next()
                    .ok_or("patch must contain one unified file diff")?
                    .map_err(|_| "invalid unified diff")?;
                let patch = file_patch
                    .patch()
                    .as_text()
                    .ok_or("only text patches are supported")?;
                let updated = apply(&current, patch).map_err(|_| ToolError::NeedsCorrection {
                    code: "workspace_patch_mismatch",
                    message: "No changes written. Reread the file and prepare a diff whose context matches it.".into(),
                })?;
                if updated == current {
                    return Err(ToolError::NeedsCorrection {
                        code: "workspace_patch_no_change",
                        message: "No changes written. Check that your diff contains a valid hunk and actually changes the file.".into(),
                    });
                }
                if updated.len() > READ_LIMIT as usize {
                    return Err(ToolError::Failed("patched file exceeds 256 KiB".into()));
                }
                if cancellation.is_cancelled() {
                    return Err(ToolError::Failed(
                        "tool cancelled before file update".into(),
                    ));
                }
                file.seek(SeekFrom::Start(0)).map_err(|_| {
                    ToolError::OutcomeUnknown("cannot seek workspace file; inspect target".into())
                })?;
                file.write_all(updated.as_bytes()).map_err(|_| {
                    ToolError::OutcomeUnknown("workspace patch write failed; inspect target".into())
                })?;
                file.set_len(updated.len() as u64).map_err(|_| {
                    ToolError::OutcomeUnknown(
                        "workspace patch truncate failed; inspect target".into(),
                    )
                })?;
                file.sync_all().map_err(|_| {
                    ToolError::OutcomeUnknown("workspace patch sync failed; inspect target".into())
                })?;
                json!({"path":args.path,"sha256":sha256(updated.as_bytes()),"bytesWritten":updated.len()})
            }
            Operation::Edit => edit::execute(&directory, &call.arguments, cancellation)?,
            Operation::Replace => {
                workspace::execute_replace(&directory, &call.arguments, cancellation)?
            }
        };
        Ok(ToolOutput {
            output,
            artifacts: vec![],
        })
    }
}

#[cfg(test)]
mod tests;
