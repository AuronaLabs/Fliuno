//! Stream a bounded set of regular files, in the approved order.
use super::*;

const MAX_PATHS: usize = 8;
const FILE_BUDGET: file_io::ReadBudget = (8 * 1024 * 1024, "file exceeds 8 MiB");

#[derive(serde::Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
enum Entry<'a> {
    Hashed {
        path: &'a str,
        #[serde(flatten)]
        fingerprint: Fingerprint,
    },
    Unavailable {
        path: &'a str,
    },
}

pub(crate) struct HashBatch;

impl Tool for HashBatch {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "workspace_sha256_batch".into(),
            description: "Compute SHA-256 digests and byte counts for up to eight explicitly named workspace files, at most 8 MiB each. Returns results in input order, with unavailable files reported individually.".into(),
            effect: ToolEffect::WorkspaceRead,
            input_schema: batch_paths::schema(MAX_PATHS),
        }
    }

    fn validate(&self, arguments: &Value) -> Result<(), String> {
        batch_paths::parse(arguments, MAX_PATHS, false).map(|_| ())
    }

    fn execute(
        &self,
        context: &ToolContext<'_>,
        call: &ToolCall,
        cancellation: &CancellationToken,
    ) -> Result<ToolOutput, ToolError> {
        let paths = batch_paths::parse(&call.arguments, MAX_PATHS, false)?;
        check_cancelled(cancellation)?;
        let directory = Dir::open_ambient_dir(&context.workspace.root, ambient_authority())
            .map_err(|_| "cannot open workspace capability")?;
        let mut options = file_io::options(false);
        options.follow(FollowSymlinks::No);
        let mut entries = Vec::with_capacity(paths.len());
        for path in paths {
            check_cancelled(cancellation)?;
            let result = directory
                .open_with(path, &options)
                .map_err(|_| ToolError::from("cannot open workspace file"))
                .and_then(|file| fingerprint(file, FILE_BUDGET, cancellation));
            // A cancelled read aborts the batch instead of reporting partial success.
            check_cancelled(cancellation)?;
            entries.push(match result {
                Ok(fingerprint) => Entry::Hashed { path, fingerprint },
                Err(_) => Entry::Unavailable { path },
            });
        }
        Ok(ToolOutput {
            output: json!({"entries":entries}),
            artifacts: Vec::new(),
        })
    }
}

fn check_cancelled(cancellation: &CancellationToken) -> Result<(), ToolError> {
    if cancellation.is_cancelled() {
        return Err("tool cancelled during workspace hashing".into());
    }
    Ok(())
}
