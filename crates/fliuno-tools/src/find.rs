//! Bounded discovery for projects where the model does not know the filenames.
use super::*;
use cap_fs_ext::DirExt;
use std::collections::VecDeque;

const MAX_ENTRIES: usize = 4096;
const MAX_TEXT_FILES: usize = 128;
const MAX_DEPTH: usize = 12;
const MAX_RESULTS: usize = 40;
const MAX_RESULT_PATH: usize = 1024;

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Mode {
    Name,
    Text,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Args {
    path: String,
    mode: Mode,
    query: String,
    limit: Option<usize>,
}

impl Args {
    fn parse(arguments: &Value) -> Result<Self, String> {
        let args: Self = serde_json::from_value(arguments.clone())
            .map_err(|_| "expected path, mode (name or text), query and optional limit")?;
        relative(&args.path, true)?;
        if args.path != "." && args.path.split('/').any(excluded) {
            return Err("project search does not enter hidden or generated directories".into());
        }
        if args.query.is_empty()
            || args.query.len() > 256
            || args.query.chars().any(char::is_control)
            || args
                .limit
                .is_some_and(|limit| !(1..=MAX_RESULTS).contains(&limit))
        {
            return Err("query must contain 1..256 bytes on one line; limit must be 1..40".into());
        }
        Ok(args)
    }
}

fn excluded(name: &str) -> bool {
    name.starts_with('.')
        || matches!(
            name.to_ascii_lowercase().as_str(),
            "target" | "node_modules" | "vendor" | "dist" | "build" | "__pycache__"
        )
}

fn project_text(name: &str) -> bool {
    if matches!(name, "Makefile" | "Dockerfile" | "LICENSE" | "README") {
        return true;
    }
    matches!(
        name.rsplit_once('.').map(|(_, ext)| ext),
        Some(
            "rs" | "toml"
                | "md"
                | "txt"
                | "py"
                | "js"
                | "mjs"
                | "cjs"
                | "ts"
                | "jsx"
                | "tsx"
                | "go"
                | "c"
                | "h"
                | "cc"
                | "cpp"
                | "hpp"
                | "cs"
                | "java"
                | "kt"
                | "json"
                | "yaml"
                | "yml"
                | "sh"
                | "ps1"
                | "sql"
                | "html"
                | "css"
                | "svelte"
                | "vue"
                | "proto"
                | "zig"
        )
    )
}

fn directory(root: &Dir, path: &str) -> std::io::Result<Dir> {
    let mut dir = root.try_clone()?;
    if path != "." {
        for part in path.split('/') {
            dir = dir.open_dir_nofollow(part)?;
        }
    }
    Ok(dir)
}

pub(super) struct Find;

impl Tool for Find {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "workspace_find".into(),
            description: "Locate project files when you do not know their paths. Use path '.' or a known subdirectory. Mode name matches a case-sensitive literal in filenames; mode text finds a case-sensitive literal in common source, config and documentation files and returns the first matching line per file, without file contents. Bounded scan skips hidden/generated directories, links, oversized files and unsupported types. If coverage is limited, search a narrower subtree. Use workspace_read_text to inspect a hit.".into(),
            effect: ToolEffect::WorkspaceRead,
            input_schema: json!({"type":"object", "required":["path","mode","query"], "additionalProperties":false,
                "properties":{
                    "path":{"type":"string","description":"Workspace-relative directory, or '.' for the root"},
                    "mode":{"type":"string","enum":["name","text"]},
                    "query":{"type":"string","description":"Case-sensitive literal, not a regular expression","minLength":1,"maxLength":256},
                    "limit":{"type":"integer","minimum":1,"maximum":MAX_RESULTS}
                }}),
        }
    }

    fn validate(&self, arguments: &Value) -> Result<(), String> {
        Args::parse(arguments).map(|_| ())
    }

    fn execute(
        &self,
        context: &ToolContext<'_>,
        call: &ToolCall,
        cancellation: &CancellationToken,
    ) -> Result<ToolOutput, ToolError> {
        let args = Args::parse(&call.arguments)?;
        check_cancelled(cancellation)?;
        let root = Dir::open_ambient_dir(&context.workspace.root, ambient_authority())
            .map_err(|_| "cannot open workspace capability")?;
        let scope = directory(&root, &args.path).map_err(|_| "cannot open search directory")?;
        let limit = args.limit.unwrap_or(20);
        let mut pending = VecDeque::from([(String::from("."), 0)]);
        let mut hits = Vec::new();
        let (mut entries, mut files, mut unreadable) = (0, 0, 0);
        let mut truncated = false;
        'scan: while let Some((prefix, depth)) = pending.pop_front() {
            check_cancelled(cancellation)?;
            let Ok(dir) = directory(&scope, &prefix) else {
                unreadable += 1;
                continue;
            };
            let Ok(children) = dir.entries() else {
                if depth == 0 {
                    return Err("cannot enumerate search directory".into());
                }
                unreadable += 1;
                continue;
            };
            for entry in children {
                check_cancelled(cancellation)?;
                if entries == MAX_ENTRIES {
                    truncated = true;
                    break 'scan;
                }
                entries += 1;
                let Ok(entry) = entry else {
                    unreadable += 1;
                    continue;
                };
                let name = entry.file_name();
                let Some(name) = name.to_str() else { continue };
                if excluded(name) || name.contains('/') || relative(name, false).is_err() {
                    continue;
                }
                let path = if prefix == "." {
                    name.into()
                } else {
                    format!("{prefix}/{name}")
                };
                let workspace_path = if args.path == "." {
                    path.clone()
                } else {
                    format!("{}/{path}", args.path)
                };
                if workspace_path.len() > MAX_RESULT_PATH {
                    truncated = true;
                    continue;
                }
                let Ok(kind) = entry.file_type() else {
                    unreadable += 1;
                    continue;
                };
                if kind.is_dir() {
                    if depth == MAX_DEPTH {
                        truncated = true;
                    } else {
                        pending.push_back((path, depth + 1));
                    }
                    continue;
                }
                if !kind.is_file() {
                    continue;
                }
                let hit = match args.mode {
                    Mode::Name => name
                        .contains(&args.query)
                        .then(|| json!({"path":workspace_path})),
                    Mode::Text if project_text(name) => {
                        if files == MAX_TEXT_FILES {
                            truncated = true;
                            break 'scan;
                        }
                        files += 1;
                        let result =
                            search::search_file(&dir, name, &args.query, &mut 1, cancellation)?;
                        if result["status"] == "unreadable" {
                            unreadable += 1;
                        }
                        result["matches"].get(0).map(|first| json!({"path":workspace_path,"line":first["line"],"byteColumn":first["byteColumn"]}))
                    }
                    Mode::Text => None,
                };
                if let Some(hit) = hit {
                    if hits.len() == limit {
                        truncated = true;
                        break 'scan;
                    }
                    hits.push(hit);
                }
            }
        }
        Ok(ToolOutput {
            output: json!({"path":args.path,"matches":hits,"visitedEntries":entries,"examinedTextFiles":files,"unreadable":unreadable,"truncated":truncated}),
            artifacts: Vec::new(),
        })
    }
}

fn check_cancelled(cancellation: &CancellationToken) -> Result<(), ToolError> {
    if cancellation.is_cancelled() {
        return Err("tool cancelled during project search".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests;
