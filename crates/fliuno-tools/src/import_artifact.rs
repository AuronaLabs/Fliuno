//! Copy a bounded, approved workspace file to the task's own artifact store.
use super::*;
use fliuno_protocol::{Artifact, ArtifactBody, MAX_BINARY_ARTIFACT_BYTES};

const MEDIA_TYPE: &str = "application/octet-stream";
const MAX_TITLE_BYTES: usize = 256;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ImportArgs<'a> {
    path: &'a str,
    title: &'a str,
}

fn parse(value: &Value) -> Result<ImportArgs<'_>, String> {
    let args = ImportArgs::deserialize(value)
        .map_err(|_| "expected a workspace path and artifact title")?;
    relative(args.path, false)?;
    if args.title.trim().is_empty()
        || args.title.len() > MAX_TITLE_BYTES
        || args.title.chars().any(char::is_control)
    {
        return Err(
            "artifact title must be 1 to 256 UTF-8 bytes without control characters".into(),
        );
    }
    Ok(args)
}

pub(super) struct ImportArtifact;

impl Tool for ImportArtifact {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "workspace_import_artifact".into(),
            description: "Save the raw bytes of a named workspace file up to 256 KiB as a task artifact. Returns its ID, size and SHA-256 for a client to read on demand.".into(),
            // Authority governs reading the named workspace path. The artifact
            // is attached only to the task that asked for the approved read.
            effect: ToolEffect::WorkspaceRead,
            input_schema: json!({
                "type":"object", "required":["path","title"], "additionalProperties":false,
                "properties":{"path":{"type":"string"},"title":{"type":"string","minLength":1,"maxLength":MAX_TITLE_BYTES}}
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
        let mut options = file_io::options(false);
        options.follow(FollowSymlinks::No);
        let mut file = directory
            .open_with(args.path, &options)
            .map_err(|_| "cannot open workspace file")?;
        let bytes = file_io::read(&mut file, cancellation)?;
        if bytes.len() > MAX_BINARY_ARTIFACT_BYTES {
            return Err("file exceeds the binary artifact byte limit".into());
        }
        let id = format!("artifact_{}", uuid::Uuid::new_v4());
        Ok(ToolOutput {
            output: json!({"artifactId":id,"contentBytes":bytes.len(),"mediaType":MEDIA_TYPE,"sha256":format!("{:x}", Sha256::digest(&bytes))}),
            artifacts: vec![Artifact {
                id,
                task_id: context.task_id.into(),
                title: args.title.into(),
                media_type: MEDIA_TYPE.into(),
                body: ArtifactBody::Binary { bytes },
            }],
        })
    }
}
