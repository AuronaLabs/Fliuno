//! Present observed project actions without exposing tool arguments or file contents.
use super::*;
use fliuno_protocol::ToolCall;
use serde_json::Value;

pub(super) fn describe(
    call: &ToolCall,
    plan: Option<&Value>,
    language: Language,
) -> Option<String> {
    let path = || call.arguments["path"].as_str().map(one_line);
    Some(match call.name.as_str() {
        "workspace_find" => match path()?.as_str() {
            "." => language.text("Search project files", "搜索项目文件").into(),
            path => format!(
                "{} {path}",
                language.text("Search project files in", "搜索目录")
            ),
        },
        "workspace_read_text" | "workspace_read_bytes" => {
            format!("{}: {}", language.text("Read file", "读取文件"), path()?)
        }
        "workspace_patch_text" => {
            format!("{}: {}", language.text("Patch file", "应用补丁"), path()?)
        }
        "workspace_edit_text" => format!("{}: {}", language.text("Edit file", "编辑文件"), path()?),
        "workspace_create_text" | "workspace_replace_text" => {
            format!("{}: {}", language.text("Write file", "写入文件"), path()?)
        }
        name if name.starts_with("process_") => {
            let plan = plan.filter(|plan| plan["kind"] == "project_check")?;
            format!(
                "{}: {}",
                language.text("Run project check", "运行项目检查"),
                one_line(plan["label"].as_str()?)
            )
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
        let language = self.language;
        let discovery = last_search(task);
        let paths = saved_workspace_paths(task);
        let checks = project_check_receipts(task, language);
        let correction = tool_receipts(task)
            .rev()
            .any(|(call, output)| unapplied_edit(call, output, language).is_some());
        let reviewable = correction || !paths.is_empty() || !checks.is_empty();
        if discovery.is_none() && !reviewable {
            return;
        }
        lines.push(Line::raw(""));
        let review = if !reviewable {
            language.text("Project activity", "项目进展")
        } else if matches!(self.layer, Layer::Edit(Edit::Task)) {
            language.text(
                "Project activity  ·  F1 changes",
                "项目进展  ·  F1 查看改动",
            )
        } else {
            language.text("Project activity  ·  d Review", "项目进展  ·  d 审阅")
        };
        lines.push(Line::styled(review, self.palette.label));
        if let Some((count, limited)) = discovery {
            let coverage = if limited {
                language.text(" · scan limited", " · 扫描范围有限")
            } else {
                ""
            };
            let matches = if count == 1 { "match" } else { "matches" };
            lines.push(Line::styled(
                match language {
                    Language::English => {
                        format!("Searched project files: {count} {matches}{coverage}")
                    }
                    Language::SimplifiedChinese => {
                        format!("已搜索项目文件：{count} 个匹配{coverage}")
                    }
                },
                self.palette.quiet,
            ));
        }
        if correction && paths.is_empty() {
            lines.push(Line::styled(
                language.text("Edit rejected; no file written", "编辑未成功，未写入文件"),
                self.palette.quiet,
            ));
        }
        for path in paths {
            lines.push(Line::styled(
                format!(
                    "{}: {}",
                    language.text("Saved in workspace", "已保存到工作区"),
                    one_line(path)
                ),
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
            || empty_review(&self.palette, self.language),
            |detail| project_review(&detail.task, &self.palette, self.language),
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
fn unapplied_edit<'a>(
    call: &'a ToolCall,
    output: &Value,
    language: Language,
) -> Option<(&'a str, &'static str)> {
    if !matches!(
        call.name.as_str(),
        "workspace_patch_text" | "workspace_edit_text"
    ) || output["status"] != "not_applied"
    {
        return None;
    }
    let reason = match output["code"].as_str()? {
        "workspace_patch_format" => language.text("Invalid diff format", "补丁格式无效"),
        "workspace_patch_mismatch" => {
            language.text("Diff did not match the file", "补丁与文件不匹配")
        }
        "workspace_patch_no_change" => {
            language.text("Diff did not change the file", "补丁未改动文件")
        }
        "workspace_base_changed" => {
            language.text("File changed after the proposal", "文件在提案后发生变化")
        }
        "workspace_edit_not_found" => language.text("Original text did not match", "未匹配到原文"),
        "workspace_edit_ambiguous" => {
            language.text("Original text matched more than once", "原文匹配到多处")
        }
        _ => return None,
    };
    Some((call.arguments["path"].as_str()?, reason))
}

struct ProjectCheckReceipt {
    summary: String,
    passed: bool,
    command: Option<String>,
}

fn project_check_receipts(task: &Task, language: Language) -> Vec<ProjectCheckReceipt> {
    let mut checks: Vec<_> = tool_receipts(task)
        .rev()
        .filter_map(|(call, output)| project_check_receipt(call, output, language))
        .take(3)
        .collect();
    checks.reverse();
    checks
}

fn project_check_receipt(
    call: &ToolCall,
    output: &Value,
    language: Language,
) -> Option<ProjectCheckReceipt> {
    let plan = &output["projectCheck"];
    if !call.name.starts_with("process_")
        || plan["kind"] != "project_check"
        || call.arguments["inputs"] != plan["inputs"]
    {
        return None;
    }
    let passed = output["success"].as_bool()? && output["termination"] == "exited";
    let reason = match output["termination"].as_str()? {
        "exited" => output["exitCode"].as_i64().map_or_else(
            || language.text("no exit code", "无退出码").into(),
            |code| format!("{} {code}", language.text("exit", "退出码")),
        ),
        "timed_out" => language.text("timed out", "超时").into(),
        "cancelled" => language.text("cancelled", "已取消").into(),
        "io_error" => language.text("output error", "输出错误").into(),
        "cleanup_failed" => language.text("cleanup failed", "清理失败").into(),
        _ => return None,
    };
    let state = if passed {
        language.text("Check passed", "检查通过")
    } else {
        language.text("Check failed", "检查失败")
    };
    let label = one_line(plan["label"].as_str()?);
    Some(ProjectCheckReceipt {
        summary: format!("{state}: {label}  ·  {reason}"),
        passed,
        command: project_check_command(plan).map(|command| clean(&command)),
    })
}

fn project_review(task: &Task, palette: &Palette, language: Language) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    for (call, output) in tool_receipts(task) {
        if let Some(path) = workspace_write_path(call, output) {
            let patch = call.arguments["patch"]
                .as_str()
                .filter(|_| call.name == "workspace_patch_text");
            let operation = match call.name.as_str() {
                "workspace_patch_text" => language.text("Applied patch", "已应用补丁"),
                "workspace_edit_text" => language.text("Replaced text", "已替换文本"),
                _ => language.text("Wrote file", "已写入文件"),
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
                    (
                        "oldText",
                        language.text("Before", "修改前"),
                        '-',
                        Color::Red,
                    ),
                    (
                        "newText",
                        language.text("After", "修改后"),
                        '+',
                        Color::Green,
                    ),
                ] {
                    if let Some(text) = call.arguments[key].as_str() {
                        lines.push(Line::styled(label, palette.quiet));
                        if text.is_empty() {
                            lines.push(Line::styled(
                                language.text("(removed)", "（已移除）"),
                                palette.quiet,
                            ));
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
        } else if let Some((path, reason)) = unapplied_edit(call, output, language) {
            lines.push(Line::styled(
                format!(
                    "{}: {}",
                    if call.name == "workspace_patch_text" {
                        language.text("Patch not applied", "补丁未应用")
                    } else {
                        language.text("Edit not applied", "编辑未应用")
                    },
                    one_line(path)
                ),
                palette.status(Color::Yellow),
            ));
            lines.push(Line::styled(reason, palette.quiet));
            lines.push(Line::default());
        } else if let Some(check) = project_check_receipt(call, output, language) {
            let color = if check.passed {
                Color::Green
            } else {
                Color::Red
            };
            lines.push(Line::styled(check.summary, palette.status(color)));
            if let Some(command) = check.command {
                lines.push(Line::styled(command, palette.quiet));
            }
            for (key, label) in [
                ("stdout", language.text("Output", "输出")),
                ("stderr", language.text("Errors", "错误输出")),
            ] {
                let capture = &output[key];
                let Some(bytes) = capture["bytesSeen"].as_u64().filter(|&bytes| bytes > 0) else {
                    continue;
                };
                lines.push(Line::styled(
                    format!("{label} · {bytes} {}", language.text("bytes", "字节")),
                    palette.label,
                ));
                if capture["truncated"] == true {
                    lines.push(Line::styled(
                        language.text("Showing only the retained tail.", "仅显示保留的末尾内容。"),
                        palette.quiet,
                    ));
                }
                if capture["encoding"] == "utf8"
                    && let Some(text) = capture["data"].as_str()
                {
                    lines.extend(clean(text).lines().map(|line| Line::raw(line.to_owned())));
                } else {
                    lines.push(Line::styled(
                        language.text("Non-text output.", "非文本输出。"),
                        palette.quiet,
                    ));
                }
            }
            lines.push(Line::default());
        }
    }
    if lines.is_empty() {
        return empty_review(palette, language);
    }
    lines.insert(
        0,
        Line::styled(
            language.text("This turn · Recorded actions", "本轮 · 已记录的操作"),
            palette.quiet,
        ),
    );
    lines
}

fn empty_review(palette: &Palette, language: Language) -> Vec<Line<'static>> {
    vec![Line::styled(
        language.text(
            "No recorded writes or checks for this turn.",
            "本轮没有已记录的写入或检查。",
        ),
        palette.quiet,
    )]
}
