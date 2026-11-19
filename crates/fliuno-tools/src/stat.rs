//! Inspect one workspace entry through the directory capability.
mod batch;
pub(super) use batch::StatBatch;

use super::*;
use std::time::UNIX_EPOCH;

pub(super) struct Stat;

fn parse(arguments: &Value) -> Result<FileArgs, String> {
    let args = FileArgs::deserialize(arguments).map_err(|_| "expected a workspace path")?;
    relative(&args.path, true)?;
    Ok(args)
}

impl Tool for Stat {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "workspace_stat".into(),
            description: "Inspect a workspace path's kind, file size, and modification time without reading its contents. Use '.' for the workspace root.".into(),
            effect: ToolEffect::WorkspaceRead,
            input_schema: json!({
                "type":"object", "required":["path"], "additionalProperties":false,
                "properties":{"path":{"type":"string"}}
            }),
        }
    }

    fn validate(&self, arguments: &Value) -> Result<(), String> {
        parse(arguments).map(|_| ())
    }

    fn execute(
        &self,
        context: &ToolContext<'_>,
        call: &ToolCall,
        cancellation: &CancellationToken,
    ) -> Result<ToolOutput, ToolError> {
        let args = parse(&call.arguments)?;
        if cancellation.is_cancelled() {
            return Err("tool cancelled before workspace access".into());
        }
        let directory = Dir::open_ambient_dir(&context.workspace.root, ambient_authority())
            .map_err(|_| "cannot open workspace capability")?;
        Ok(ToolOutput {
            output: json!(
                inspect(&directory, &args.path).map_err(|_| "cannot inspect workspace path")?
            ),
            artifacts: Vec::new(),
        })
    }
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct Entry<'a> {
    path: &'a str,
    kind: &'static str,
    bytes: Option<u64>,
    modified_unix_ms: Option<u64>,
}

fn inspect<'a>(directory: &Dir, path: &'a str) -> std::io::Result<Entry<'a>> {
    let metadata = if path == "." {
        directory.dir_metadata()
    } else {
        directory.symlink_metadata(path)
    }?;
    let kind = if metadata.is_symlink() {
        "symlink"
    } else if metadata.is_dir() {
        "directory"
    } else if metadata.is_file() {
        "file"
    } else {
        "other"
    };
    let modified_unix_ms = metadata
        .modified()
        .ok()
        .and_then(|time| time.into_std().duration_since(UNIX_EPOCH).ok())
        .and_then(|duration| u64::try_from(duration.as_millis()).ok());
    Ok(Entry {
        path,
        kind,
        bytes: metadata.is_file().then(|| metadata.len()),
        modified_unix_ms,
    })
}
