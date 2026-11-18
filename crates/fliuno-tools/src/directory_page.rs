//! Ordered pages over a bounded directory scan. The cursor is only a name
//! boundary and never grants access to a path or carries an open handle.
use super::*;
use std::collections::BinaryHeap;

const MAX_VISITED: usize = 16_384;
const MAX_PAGE_SIZE: usize = 256;
const DEFAULT_PAGE_SIZE: usize = 128;
const MAX_NAME_BYTES: usize = 1024;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Args {
    path: String,
    after: Option<String>,
    limit: Option<usize>,
}

impl Args {
    fn parse(arguments: &Value) -> Result<Self, String> {
        let args: Self = serde_json::from_value(arguments.clone())
            .map_err(|_| "expected path and optional after and limit")?;
        relative(&args.path, true)?;
        if args.after.as_deref().is_some_and(|name| !addressable(name)) {
            return Err("after must be a valid entry name".into());
        }
        if args
            .limit
            .is_some_and(|limit| !(1..=MAX_PAGE_SIZE).contains(&limit))
        {
            return Err("directory page limit must be 1..256".into());
        }
        Ok(args)
    }
}

fn addressable(name: &str) -> bool {
    name.len() <= MAX_NAME_BYTES && !name.contains('/') && relative(name, false).is_ok()
}

pub(super) struct ListPage;

impl Tool for ListPage {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "workspace_list_page".into(),
            description: "List a page of sorted entries in a workspace directory. Pass the preceding nextAfter value as after. Use '.' for the workspace root.".into(),
            effect: ToolEffect::WorkspaceRead,
            input_schema: json!({
                "type":"object", "required":["path"], "additionalProperties":false,
                "properties": {
                    "path":{"type":"string"},
                    "after":{"type":"string","minLength":1,"maxLength":MAX_NAME_BYTES},
                    "limit":{"type":"integer","minimum":1,"maximum":MAX_PAGE_SIZE,"default":DEFAULT_PAGE_SIZE}
                }
            }),
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
        if cancellation.is_cancelled() {
            return Err("tool cancelled before directory access".into());
        }
        let directory = Dir::open_ambient_dir(&context.workspace.root, ambient_authority())
            .map_err(|_| "cannot open workspace capability")?;
        let results = directory
            .read_dir(&args.path)
            .map_err(|_| "cannot list workspace path")?;
        let limit = args.limit.unwrap_or(DEFAULT_PAGE_SIZE);
        let mut page: BinaryHeap<(String, &'static str)> = BinaryHeap::with_capacity(limit);
        let mut has_more = false;
        let mut omitted_names = 0;
        for (index, entry) in results.enumerate() {
            if cancellation.is_cancelled() {
                return Err("tool cancelled".into());
            }
            if index == MAX_VISITED {
                return Err(
                    "directory exceeds the 16384-entry scan limit; select a smaller directory"
                        .into(),
                );
            }
            let entry = entry.map_err(|_| "cannot read directory entry")?;
            let Ok(name) = entry.file_name().into_string() else {
                omitted_names += 1;
                continue;
            };
            if !addressable(&name) {
                omitted_names += 1;
                continue;
            }
            if args.after.as_ref().is_some_and(|after| &name <= after) {
                continue;
            }
            if page.len() == limit {
                has_more = true;
                if page.peek().is_some_and(|(largest, _)| &name >= largest) {
                    continue;
                }
                page.pop();
            }
            let file_type = entry.file_type().map_err(|_| "cannot read entry type")?;
            let kind = if file_type.is_symlink() {
                "symlink"
            } else if file_type.is_dir() {
                "directory"
            } else if file_type.is_file() {
                "file"
            } else {
                "other"
            };
            page.push((name, kind));
        }
        let page = page.into_sorted_vec();
        let next_after = if has_more {
            page.last().map(|(name, _)| name)
        } else {
            None
        };
        let entries: Vec<_> = page
            .iter()
            .map(|(name, kind)| json!({"name":name,"kind":kind}))
            .collect();
        Ok(ToolOutput {
            output: json!({"entries":entries,"nextAfter":next_after,"hasMore":has_more,"omittedNames":omitted_names}),
            artifacts: Vec::new(),
        })
    }
}
