//! Present observed project actions without exposing tool arguments or file contents.
use super::*;
use fliuno_protocol::ToolCall;
use serde_json::Value;

pub(super) fn describe(call: &ToolCall, plan: Option<&Value>) -> Option<String> {
    let path = || call.arguments["path"].as_str().map(one_line);
    Some(match call.name.as_str() {
        "workspace_find" => match path()?.as_str() {
            "." => "Search project files".into(),
            path => format!("Search project files in {path}"),
        },
        "workspace_read_text" | "workspace_read_bytes" => format!("Read file: {}", path()?),
        "workspace_patch_text" => format!("Patch file: {}", path()?),
        "workspace_create_text" | "workspace_replace_text" => format!("Write file: {}", path()?),
        name if name.starts_with("process_") => {
            let plan = plan.filter(|plan| plan["kind"] == "project_check")?;
            format!("Run project check: {}", one_line(plan["label"].as_str()?))
        }
        _ => return None,
    })
}

fn one_line(text: &str) -> String {
    clean(text).replace('\n', " ")
}

impl App {
    pub(super) fn append_project_activity(
        &self,
        task: &Task,
        lines: &mut Vec<Line<'static>>,
        width: u16,
    ) {
        let discovery = last_search(task);
        let paths = saved_workspace_paths(task);
        let checks = project_check_receipts(task);
        if discovery.is_none() && paths.is_empty() && checks.is_empty() {
            return;
        }
        lines.push(Line::raw(""));
        lines.push(Line::styled("Project activity", self.palette.label));
        if let Some((count, limited)) = discovery {
            let coverage = if limited { " · scan limited" } else { "" };
            let matches = if count == 1 { "match" } else { "matches" };
            lines.push(Line::styled(
                format!("Searched project files: {count} {matches}{coverage}"),
                self.palette.quiet,
            ));
        }
        for path in paths {
            lines.push(Line::styled(
                format!("Saved in workspace: {}", one_line(path)),
                self.palette.base,
            ));
        }
        for check in checks {
            let style = self.palette.status(if check.passed {
                Color::Green
            } else {
                Color::Red
            });
            lines.push(Line::styled(check.summary, style));
            if (self.show_details || width >= 76)
                && let Some(command) = check.command
            {
                lines.push(Line::styled(command, self.palette.quiet));
            }
        }
    }
}

fn last_search(task: &Task) -> Option<(usize, bool)> {
    task.messages.iter().rev().find_map(|message| {
        let Message::ToolResult { call_id, output } = message else {
            return None;
        };
        let matches = output["matches"].as_array()?;
        let limited = output["truncated"].as_bool()? || output["unreadable"].as_u64()? > 0;
        task.messages
            .iter()
            .any(|message| {
                matches!(message,
                    Message::ToolCall { call }
                        if call.id == *call_id && call.name == "workspace_find"
                            && call.arguments["path"] == output["path"]
                )
            })
            .then_some((matches.len(), limited))
    })
}

/// Only pair results emitted by the kernel with the matching workspace write.
/// Model prose, pending approvals and unknown outcomes cannot create a receipt.
fn saved_workspace_paths(task: &Task) -> Vec<&str> {
    let mut paths = Vec::new();
    for message in task.messages.iter().rev() {
        let Message::ToolResult { call_id, output } = message else {
            continue;
        };
        if output["bytesWritten"].as_u64().is_none() {
            continue;
        }
        let Some(path) = output["path"].as_str() else {
            continue;
        };
        if !paths.contains(&path)
            && task.messages.iter().any(|message| {
                matches!(message, Message::ToolCall { call }
                if call.id == *call_id && matches!(call.name.as_str(),
                    "workspace_create_text" | "workspace_replace_text" | "workspace_patch_text")
                    && call.arguments["path"].as_str() == Some(path))
            })
        {
            paths.push(path);
            if paths.len() == 5 {
                break;
            }
        }
    }
    paths.reverse();
    paths
}

struct ProjectCheckReceipt {
    summary: String,
    passed: bool,
    command: Option<String>,
}

fn project_check_receipts(task: &Task) -> Vec<ProjectCheckReceipt> {
    task.messages
        .iter()
        .rev()
        .filter_map(|message| {
            let Message::ToolResult { call_id, output } = message else {
                return None;
            };
            let plan = &output["projectCheck"];
            if plan["kind"] != "project_check" || output["termination"] != "exited" {
                return None;
            }
            let call = task.messages.iter().find_map(|message| match message {
                Message::ToolCall { call }
                    if &call.id == call_id && call.name.starts_with("process_") =>
                {
                    Some(call)
                }
                _ => None,
            })?;
            if call.arguments["inputs"] != plan["inputs"] {
                return None;
            }
            let passed = output["success"].as_bool()?;
            let state = if passed {
                "Check passed"
            } else {
                "Check failed"
            };
            let exit = output["exitCode"]
                .as_i64()
                .map_or_else(|| "no exit code".into(), |code| format!("exit {code}"));
            let label = clean(plan["label"].as_str()?).replace('\n', " ");
            Some(ProjectCheckReceipt {
                summary: format!("{state}: {label}  ·  {exit}"),
                passed,
                command: project_check_command(plan).map(|command| clean(&command)),
            })
        })
        .take(3)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect()
}
