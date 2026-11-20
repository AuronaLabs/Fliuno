//! Inspect a fixed list of paths under one explicit approval.
use super::*;

const MAX_PATHS: usize = 16;

#[derive(serde::Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
enum BatchEntry<'a> {
    Found {
        #[serde(flatten)]
        entry: Entry<'a>,
    },
    Unavailable {
        path: &'a str,
    },
}

pub(crate) struct StatBatch;

impl Tool for StatBatch {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "workspace_stat_batch".into(),
            description: "Inspect kind, size, and modification time for up to 16 explicitly named workspace paths in one call. Reports unavailable paths individually and reads no file contents.".into(),
            effect: ToolEffect::WorkspaceRead,
            input_schema: batch_paths::schema(MAX_PATHS),
        }
    }

    fn validate(&self, arguments: &Value) -> Result<(), String> {
        batch_paths::parse(arguments, MAX_PATHS, true).map(|_| ())
    }

    fn execute(
        &self,
        context: &ToolContext<'_>,
        call: &ToolCall,
        cancellation: &CancellationToken,
    ) -> Result<ToolOutput, ToolError> {
        let paths = batch_paths::parse(&call.arguments, MAX_PATHS, true)?;
        check_cancelled(cancellation)?;
        let directory = Dir::open_ambient_dir(&context.workspace.root, ambient_authority())
            .map_err(|_| "cannot open workspace capability")?;
        let mut entries = Vec::with_capacity(paths.len());
        for path in paths {
            check_cancelled(cancellation)?;
            entries.push(match inspect(&directory, path) {
                Ok(entry) => BatchEntry::Found { entry },
                Err(_) => BatchEntry::Unavailable { path },
            });
        }
        check_cancelled(cancellation)?;
        Ok(ToolOutput {
            output: json!({"entries":entries}),
            artifacts: Vec::new(),
        })
    }
}

fn check_cancelled(cancellation: &CancellationToken) -> Result<(), ToolError> {
    if cancellation.is_cancelled() {
        return Err("tool cancelled during workspace metadata inspection".into());
    }
    Ok(())
}
