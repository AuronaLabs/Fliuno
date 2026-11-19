//! Inspect a fixed list of paths under one explicit approval.
use super::*;

const MAX_PATHS: usize = 16;
const MAX_TOTAL_PATH_BYTES: usize = 8 * 1024;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BatchArgs<'a> {
    #[serde(borrow)]
    paths: Vec<&'a str>,
}

fn parse(arguments: &Value) -> Result<BatchArgs<'_>, String> {
    let args = BatchArgs::deserialize(arguments).map_err(|_| "expected workspace paths")?;
    if !(1..=MAX_PATHS).contains(&args.paths.len()) {
        return Err("paths must contain 1..16 workspace paths".into());
    }
    for (index, path) in args.paths.iter().enumerate() {
        relative(path, true)?;
        if args.paths[..index].contains(path) {
            return Err("paths must be unique".into());
        }
    }
    if args.paths.iter().map(|path| path.len()).sum::<usize>() > MAX_TOTAL_PATH_BYTES {
        return Err("paths exceed the 8 KiB combined byte limit".into());
    }
    Ok(args)
}

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
            input_schema: json!({
                "type":"object", "required":["paths"], "additionalProperties":false,
                "properties":{"paths":{"type":"array","items":{"type":"string"},"minItems":1,"maxItems":MAX_PATHS,"uniqueItems":true}}
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
        check_cancelled(cancellation)?;
        let directory = Dir::open_ambient_dir(&context.workspace.root, ambient_authority())
            .map_err(|_| "cannot open workspace capability")?;
        let mut entries = Vec::with_capacity(args.paths.len());
        for path in args.paths {
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
