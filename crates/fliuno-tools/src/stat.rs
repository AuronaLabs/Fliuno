//! Inspect one workspace entry through the directory capability.
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
        let metadata = if args.path == "." {
            directory.dir_metadata()
        } else {
            directory.symlink_metadata(&args.path)
        }
        .map_err(|_| "cannot inspect workspace path")?;
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
        Ok(ToolOutput {
            output: json!({
                "path":args.path, "kind":kind,
                "bytes":metadata.is_file().then(|| metadata.len()),
                "modifiedUnixMs":modified_unix_ms
            }),
            artifacts: Vec::new(),
        })
    }
}
