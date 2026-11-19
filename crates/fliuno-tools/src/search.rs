//! Locate text in explicitly approved files with bounded input and output.
use super::*;

const MAX_FILES: usize = 8;
const MAX_QUERY_BYTES: usize = 256;
const MAX_MATCHES: usize = 64;
const DEFAULT_MATCHES: usize = 24;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SearchArgs {
    paths: Vec<String>,
    query: String,
    max_matches: Option<usize>,
}

impl SearchArgs {
    fn parse(arguments: &Value) -> Result<Self, String> {
        let args = Self::deserialize(arguments)
            .map_err(|_| "expected paths, query and optional maxMatches")?;
        if !(1..=MAX_FILES).contains(&args.paths.len()) {
            return Err("paths must contain 1..8 workspace file paths".into());
        }
        for (index, path) in args.paths.iter().enumerate() {
            relative(path, false)?;
            if args.paths[..index].contains(path) {
                return Err("paths must be unique".into());
            }
        }
        if args.query.is_empty()
            || args.query.len() > MAX_QUERY_BYTES
            || args.query.contains(['\r', '\n', '\0'])
        {
            return Err("query must contain 1..256 UTF-8 bytes on one line without NUL".into());
        }
        if args
            .max_matches
            .is_some_and(|limit| !(1..=MAX_MATCHES).contains(&limit))
        {
            return Err("maxMatches must be 1..64".into());
        }
        Ok(args)
    }
}

pub(super) struct Search;

impl Tool for Search {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "workspace_search_text".into(),
            description: "Locate a case-sensitive literal in up to eight explicitly named UTF-8 workspace files, at most 256 KiB each. Returns line numbers and byte columns without file contents. Read specific lines with workspace_read_text.".into(),
            effect: ToolEffect::WorkspaceRead,
            input_schema: json!({
                "type":"object", "required":["paths","query"], "additionalProperties":false,
                "properties": {
                    "paths":{"type":"array","items":{"type":"string"},"minItems":1,"maxItems":MAX_FILES,"uniqueItems":true},
                    "query":{"type":"string","minLength":1,"maxLength":MAX_QUERY_BYTES},
                    "maxMatches":{"type":"integer","minimum":1,"maximum":MAX_MATCHES,"default":DEFAULT_MATCHES}
                }
            }),
        }
    }

    fn validate(&self, arguments: &Value) -> Result<(), String> {
        SearchArgs::parse(arguments).map(|_| ())
    }

    fn execute(
        &self,
        context: &ToolContext<'_>,
        call: &ToolCall,
        cancellation: &CancellationToken,
    ) -> Result<ToolOutput, ToolError> {
        let args = SearchArgs::parse(&call.arguments)?;
        check_cancelled(cancellation)?;
        let directory = Dir::open_ambient_dir(&context.workspace.root, ambient_authority())
            .map_err(|_| "cannot open workspace capability")?;
        let mut remaining = args.max_matches.unwrap_or(DEFAULT_MATCHES);
        let mut files = Vec::with_capacity(args.paths.len());
        for path in &args.paths {
            files.push(search_file(
                &directory,
                path,
                &args.query,
                &mut remaining,
                cancellation,
            )?);
        }
        check_cancelled(cancellation)?;
        Ok(ToolOutput {
            output: json!({"files":files}),
            artifacts: Vec::new(),
        })
    }
}

fn check_cancelled(cancellation: &CancellationToken) -> Result<(), ToolError> {
    if cancellation.is_cancelled() {
        return Err("tool cancelled during workspace search".into());
    }
    Ok(())
}

fn search_file(
    directory: &Dir,
    path: &str,
    query: &str,
    remaining: &mut usize,
    cancellation: &CancellationToken,
) -> Result<Value, ToolError> {
    check_cancelled(cancellation)?;
    let mut options = file_io::options(false);
    options.follow(FollowSymlinks::No);
    let bytes = directory
        .open_with(path, &options)
        .map_err(|_| ToolError::from("cannot open workspace file"))
        .and_then(|mut file| file_io::read(&mut file, cancellation));
    check_cancelled(cancellation)?;
    let bytes = match bytes {
        Ok(bytes) => bytes,
        Err(_) => return Ok(json!({"path":path,"status":"unreadable"})),
    };
    let Ok(text) = std::str::from_utf8(&bytes) else {
        return Ok(json!({"path":path,"status":"nonText"}));
    };
    if text.contains('\0') {
        return Ok(json!({"path":path,"status":"nonText"}));
    }
    let mut matches = Vec::new();
    let mut has_more = false;
    for (index, line) in text.lines().enumerate() {
        check_cancelled(cancellation)?;
        if let Some(column) = line.find(query) {
            if *remaining == 0 {
                has_more = true;
                break;
            }
            matches.push(json!({"line":index+1,"byteColumn":column+1}));
            *remaining -= 1;
        }
    }
    Ok(json!({"path":path,"status":"searched","matches":matches,"hasMore":has_more}))
}
