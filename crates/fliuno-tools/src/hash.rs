//! Fingerprint workspace files without placing their content in model context.
mod batch;
pub(super) use batch::HashBatch;

use super::*;

pub(super) const BUDGET: file_io::ReadBudget = (64 * 1024 * 1024, "file exceeds 64 MiB");

pub(super) struct Hash;

fn parse(arguments: &Value) -> Result<FileArgs, String> {
    let args = FileArgs::deserialize(arguments).map_err(|_| "expected a workspace file path")?;
    relative(&args.path, false)?;
    Ok(args)
}

impl Tool for Hash {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "workspace_sha256".into(),
            description: "Compute the SHA-256 and byte count of a regular workspace file up to 64 MiB. Only returns the digest and metadata.".into(),
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
        let file = directory
            .open_with(&args.path, &file_io::options(false))
            .map_err(|_| "cannot open workspace file")?;
        let digest = fingerprint(file, BUDGET, cancellation)?;
        Ok(ToolOutput {
            output: json!({"path":args.path, "bytes":digest.bytes, "sha256":digest.sha256}),
            artifacts: Vec::new(),
        })
    }
}

#[derive(serde::Serialize)]
struct Fingerprint {
    bytes: u64,
    sha256: String,
}

fn fingerprint(
    file: cap_std::fs::File,
    budget: file_io::ReadBudget,
    cancellation: &CancellationToken,
) -> Result<Fingerprint, ToolError> {
    file_io::require_regular_file(&file, budget, cancellation)?;
    let mut digest = Sha256::new();
    let bytes = file_io::read_chunks(file, budget, cancellation, |chunk| digest.update(chunk))?;
    Ok(Fingerprint {
        bytes,
        sha256: format!("{:x}", digest.finalize()),
    })
}
