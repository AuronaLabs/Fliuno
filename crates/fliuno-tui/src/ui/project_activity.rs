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
        "workspace_edit_text" => format!("Edit file: {}", path()?),
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
        let correction = tool_receipts(task)
            .rev()
            .any(|(call, output)| unapplied_edit(call, output).is_some());
        let reviewable = correction || !paths.is_empty() || !checks.is_empty();
        if discovery.is_none() && !reviewable {
            return;
        }
        lines.push(Line::raw(""));
        let review = if !reviewable {
            "Project activity"
        } else if matches!(self.layer, Layer::Edit(Edit::Task)) {
            "Project activity  ·  F1 changes"
        } else {
            "Project activity  ·  d Review"
        };
        lines.push(Line::styled(review, self.palette.label));
        if let Some((count, limited)) = discovery {
            let coverage = if limited { " · scan limited" } else { "" };
            let matches = if count == 1 { "match" } else { "matches" };
            lines.push(Line::styled(
                format!("Searched project files: {count} {matches}{coverage}"),
                self.palette.quiet,
            ));
        }
        if correction && paths.is_empty() {
            lines.push(Line::styled(
                "Edit rejected; no file written",
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

    pub(super) fn open_project_review(&mut self, return_to_draft: bool) {
        self.layer = Layer::ProjectReview {
            return_to_draft,
            scroll: 0,
        };
    }

    pub(super) fn draw_project_review(&self, frame: &mut Frame<'_>, area: Rect, scroll: u16) {
        let lines = self.detail.as_ref().map_or_else(
            || empty_review(&self.palette),
            |detail| project_review(&detail.task, &self.palette),
        );
        let paragraph = Paragraph::new(lines).wrap(Wrap { trim: false });
        let last_row = paragraph
            .line_count(area.width)
            .saturating_sub(usize::from(area.height));
        frame.render_widget(
            paragraph.scroll((usize::from(scroll).min(last_row) as u16, 0)),
            area,
        );
    }
}

fn last_search(task: &Task) -> Option<(usize, bool)> {
    tool_receipts(task).rev().find_map(|(call, output)| {
        if call.name != "workspace_find" || call.arguments["path"] != output["path"] {
            return None;
        }
        let matches = output["matches"].as_array()?;
        let limited = output["truncated"].as_bool()? || output["unreadable"].as_u64()? > 0;
        Some((matches.len(), limited))
    })
}

/// Only pair results emitted by the kernel with the matching workspace write.
/// Model prose, pending approvals and unknown outcomes cannot create a receipt.
fn saved_workspace_paths(task: &Task) -> Vec<&str> {
    let mut paths = Vec::new();
    for (call, output) in tool_receipts(task).rev() {
        if let Some(path) = workspace_write_path(call, output)
            && !paths.contains(&path)
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

fn tool_receipts(task: &Task) -> impl DoubleEndedIterator<Item = (&ToolCall, &Value)> {
    let calls: std::collections::HashMap<_, _> = task
        .messages
        .iter()
        .filter_map(|message| match message {
            Message::ToolCall { call } => Some((call.id.as_str(), call)),
            _ => None,
        })
        .collect();
    task.messages
        .iter()
        .filter_map(move |message| match message {
            Message::ToolResult { call_id, output } => {
                Some((*calls.get(call_id.as_str())?, output))
            }
            _ => None,
        })
}

fn workspace_write_path<'a>(call: &ToolCall, output: &'a Value) -> Option<&'a str> {
    let path = output["path"].as_str()?;
    (matches!(
        call.name.as_str(),
        "workspace_create_text"
            | "workspace_replace_text"
            | "workspace_patch_text"
            | "workspace_edit_text"
    ) && call.arguments["path"].as_str() == Some(path)
        && output["bytesWritten"].as_u64().is_some())
    .then_some(path)
}

/// Show why a proposal was rejected without presenting it as a change.
fn unapplied_edit<'a>(call: &'a ToolCall, output: &Value) -> Option<(&'a str, &'static str)> {
    if !matches!(
        call.name.as_str(),
        "workspace_patch_text" | "workspace_edit_text"
    ) || output["status"] != "not_applied"
    {
        return None;
    }
    let reason = match output["code"].as_str()? {
        "workspace_patch_format" => "Invalid diff format",
        "workspace_patch_mismatch" => "Diff did not match the file",
        "workspace_patch_no_change" => "Diff did not change the file",
        "workspace_base_changed" => "File changed after the proposal",
        "workspace_edit_not_found" => "Original text did not match",
        "workspace_edit_ambiguous" => "Original text matched more than once",
        _ => return None,
    };
    Some((call.arguments["path"].as_str()?, reason))
}

struct ProjectCheckReceipt {
    summary: String,
    passed: bool,
    command: Option<String>,
}

fn project_check_receipts(task: &Task) -> Vec<ProjectCheckReceipt> {
    let mut checks: Vec<_> = tool_receipts(task)
        .rev()
        .filter_map(|(call, output)| project_check_receipt(call, output))
        .take(3)
        .collect();
    checks.reverse();
    checks
}

fn project_check_receipt(call: &ToolCall, output: &Value) -> Option<ProjectCheckReceipt> {
    let plan = &output["projectCheck"];
    if !call.name.starts_with("process_")
        || plan["kind"] != "project_check"
        || call.arguments["inputs"] != plan["inputs"]
    {
        return None;
    }
    let passed = output["success"].as_bool()? && output["termination"] == "exited";
    let reason = match output["termination"].as_str()? {
        "exited" => output["exitCode"]
            .as_i64()
            .map_or_else(|| "no exit code".into(), |code| format!("exit {code}")),
        "timed_out" => "timed out".into(),
        "cancelled" => "cancelled".into(),
        "io_error" => "output error".into(),
        "cleanup_failed" => "cleanup failed".into(),
        _ => return None,
    };
    let state = if passed {
        "Check passed"
    } else {
        "Check failed"
    };
    let label = one_line(plan["label"].as_str()?);
    Some(ProjectCheckReceipt {
        summary: format!("{state}: {label}  ·  {reason}"),
        passed,
        command: project_check_command(plan).map(|command| clean(&command)),
    })
}

fn project_review(task: &Task, palette: &Palette) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    for (call, output) in tool_receipts(task) {
        if let Some(path) = workspace_write_path(call, output) {
            let patch = call.arguments["patch"]
                .as_str()
                .filter(|_| call.name == "workspace_patch_text");
            let operation = match call.name.as_str() {
                "workspace_patch_text" => "Applied patch",
                "workspace_edit_text" => "Replaced text",
                _ => "Wrote file",
            };
            lines.push(Line::styled(
                format!("{operation}: {}", one_line(path)),
                palette.label,
            ));
            if let Some(patch) = patch {
                lines.extend(clean(patch).lines().map(|line| {
                    let style = match line.as_bytes().first() {
                        Some(b'+') if !line.starts_with("+++") => palette.status(Color::Green),
                        Some(b'-') if !line.starts_with("---") => palette.status(Color::Red),
                        Some(b'@') => palette.label,
                        _ => palette.quiet,
                    };
                    Line::styled(line.to_owned(), style)
                }));
            }
            if call.name == "workspace_edit_text" {
                for (key, label, prefix, color) in [
                    ("oldText", "Before", '-', Color::Red),
                    ("newText", "After", '+', Color::Green),
                ] {
                    if let Some(text) = call.arguments[key].as_str() {
                        lines.push(Line::styled(label, palette.quiet));
                        if text.is_empty() {
                            lines.push(Line::styled("(removed)", palette.quiet));
                        } else {
                            lines.extend(exact_edit_text(text).split_terminator('\n').map(
                                |line| {
                                    Line::styled(format!("{prefix} {line}"), palette.status(color))
                                },
                            ));
                        }
                    }
                }
            }
            lines.push(Line::default());
        } else if let Some((path, reason)) = unapplied_edit(call, output) {
            lines.push(Line::styled(
                format!(
                    "{} not applied: {}",
                    if call.name == "workspace_patch_text" {
                        "Patch"
                    } else {
                        "Edit"
                    },
                    one_line(path)
                ),
                palette.status(Color::Yellow),
            ));
            lines.push(Line::styled(reason, palette.quiet));
            lines.push(Line::default());
        } else if let Some(check) = project_check_receipt(call, output) {
            let color = if check.passed {
                Color::Green
            } else {
                Color::Red
            };
            lines.push(Line::styled(check.summary, palette.status(color)));
            if let Some(command) = check.command {
                lines.push(Line::styled(command, palette.quiet));
            }
            for (key, label) in [("stdout", "Output"), ("stderr", "Errors")] {
                let capture = &output[key];
                let Some(bytes) = capture["bytesSeen"].as_u64().filter(|&bytes| bytes > 0) else {
                    continue;
                };
                lines.push(Line::styled(
                    format!("{label} · {bytes} bytes"),
                    palette.label,
                ));
                if capture["truncated"] == true {
                    lines.push(Line::styled(
                        "Showing only the retained tail.",
                        palette.quiet,
                    ));
                }
                if capture["encoding"] == "utf8"
                    && let Some(text) = capture["data"].as_str()
                {
                    lines.extend(clean(text).lines().map(|line| Line::raw(line.to_owned())));
                } else {
                    lines.push(Line::styled("Non-text output.", palette.quiet));
                }
            }
            lines.push(Line::default());
        }
    }
    if lines.is_empty() {
        return empty_review(palette);
    }
    lines.insert(
        0,
        Line::styled("This turn · Recorded actions", palette.quiet),
    );
    lines
}

fn empty_review(palette: &Palette) -> Vec<Line<'static>> {
    vec![Line::styled(
        "No recorded writes or checks for this turn.",
        palette.quiet,
    )]
}
