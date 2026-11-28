use super::*;
use serde_json::Value;
use std::{cell::RefCell, fmt::Write as _};

pub(super) struct Review {
    summary: Page,
    details: Page,
    show_details: bool,
    pub(super) allow_once: bool,
}

struct Page {
    text: String,
    // Byte offset that separates the explanation from verbatim content.
    prose_end: usize,
    start: Cell<usize>,
    viewport: Cell<usize>,
    wrapped: RefCell<(u16, Vec<std::ops::Range<usize>>)>,
}

impl Page {
    fn new(text: String, prose_end: usize) -> Self {
        Self {
            text,
            prose_end,
            start: Cell::new(0),
            viewport: Cell::new(1),
            wrapped: RefCell::default(),
        }
    }

    fn last_start(&self) -> usize {
        self.wrapped
            .borrow()
            .1
            .len()
            .saturating_sub(self.viewport.get())
    }

    fn navigate(&self, key: KeyCode) {
        let start = self.start.get();
        let page = self.viewport.get().saturating_sub(1).max(1);
        self.start.set(
            match key {
                KeyCode::Home => 0,
                KeyCode::End => self.last_start(),
                KeyCode::Up => start.saturating_sub(1),
                KeyCode::Down => start.saturating_add(1),
                KeyCode::PageUp => start.saturating_sub(page),
                KeyCode::PageDown => start.saturating_add(page),
                _ => return,
            }
            .min(self.last_start()),
        );
    }

    fn layout(&self, width: u16, height: u16) {
        let mut cached = self.wrapped.borrow_mut();
        let at_end = self.start.get() > 0
            && self.start.get() >= cached.1.len().saturating_sub(self.viewport.get());
        self.viewport.set(usize::from(height));
        if cached.0 == width {
            if at_end {
                self.start
                    .set(cached.1.len().saturating_sub(usize::from(height)));
            }
            return;
        }
        let anchor = cached.1.get(self.start.get()).map(|row| row.start);
        cached.0 = width;
        cached.1.clear();
        let mut offset = 0;
        for line in self.text.split('\n') {
            let (mut first, mut cells, mut whitespace) = (0, 0, None);
            let prose = offset < self.prose_end;
            for (index, cluster) in line.grapheme_indices(true) {
                let size = cluster.width();
                if cells + size > usize::from(width) && index > first {
                    let end = whitespace.filter(|&end| end > first).unwrap_or(index);
                    cached.1.push(offset + first..offset + end);
                    first = end;
                    cells = line[end..index].width();
                    // Long words and paths may still need a display-column break.
                    if cells + size > usize::from(width) && end < index {
                        cached.1.push(offset + end..offset + index);
                        first = index;
                        cells = 0;
                    }
                    whitespace = None;
                }
                cells += size;
                if prose && cluster == " " {
                    whitespace = Some(index + cluster.len());
                }
            }
            cached.1.push(offset + first..offset + line.len());
            offset += line.len() + 1;
        }
        if at_end {
            // Preserve the bottom of the preview if the terminal width changes.
            self.start.set(cached.1.len());
        } else if let Some(anchor) = anchor {
            self.start.set(
                cached
                    .1
                    .partition_point(|row| row.start <= anchor)
                    .saturating_sub(1),
            );
        }
    }
}

impl Review {
    pub(super) fn new(summary: String, details: String, allow_once: bool) -> Self {
        Self {
            summary: Page::new(summary, usize::MAX),
            details: Page::new(details, 0),
            show_details: false,
            allow_once,
        }
    }

    fn verbatim_from(mut self, byte_offset: usize) -> Self {
        self.summary.prose_end = byte_offset;
        self
    }

    #[cfg(test)]
    pub(super) fn summary(&self) -> &str {
        &self.summary.text
    }

    pub(super) fn navigate(&mut self, key: KeyCode, pressed: bool) {
        if matches!(key, KeyCode::Tab | KeyCode::BackTab)
            && pressed
            && !self.details.text.is_empty()
        {
            self.show_details = !self.show_details;
        } else {
            self.page().navigate(key);
        }
    }

    fn page(&self) -> &Page {
        if self.show_details {
            &self.details
        } else {
            &self.summary
        }
    }

    pub(super) fn draw(
        &self,
        frame: &mut Frame<'_>,
        area: Rect,
        language: Language,
        palette: &Palette,
    ) {
        if area.is_empty() {
            return;
        }
        let page = self.page();
        let [bar, content] = Layout::vertical([
            Constraint::Length(u16::from(area.height >= 3)),
            Constraint::Min(1),
        ])
        .areas(area);
        page.layout(content.width, content.height);
        let start = page.start.get().min(page.last_start());
        page.start.set(start);
        let wrapped = page.wrapped.borrow();
        let total = wrapped.1.len();
        let end = start.saturating_add(usize::from(content.height)).min(total);
        if !bar.is_empty() {
            let label = if self.show_details {
                language.text("Details · Tab Preview", "详情 · Tab 预览")
            } else if self.details.text.is_empty() {
                language.text("Preview", "预览")
            } else {
                language.text("Preview · Tab Details", "预览 · Tab 详情")
            };
            let range = if total > usize::from(content.height) {
                format!("  ·  {}–{end}/{total} ↑↓", start + 1)
            } else {
                String::new()
            };
            let range = if range.width() + 8 <= usize::from(bar.width) {
                range
            } else {
                String::new()
            };
            let room = usize::from(bar.width).saturating_sub(range.width());
            frame.render_widget(
                Line::styled(format!("{}{range}", ellipsis(label, room)), palette.quiet),
                bar,
            );
        }
        for (y, row) in wrapped.1[start..end].iter().enumerate() {
            frame.render_widget(
                Line::raw(&page.text[row.clone()]),
                Rect::new(content.x, content.y + y as u16, content.width, 1),
            );
        }
    }
}

fn display_text(text: &str) -> String {
    let mut safe = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '\n' => safe.push(c),
            '\r' | '\t' => {
                let _ = write!(safe, "{}", c.escape_default());
            }
            c if c.is_control()
                || matches!(c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}') =>
            {
                let _ = write!(safe, "{}", c.escape_unicode());
            }
            _ => safe.push(c),
        }
    }
    safe
}

fn format_details(
    name: &str,
    arguments: &Value,
    plan: Option<&Value>,
    language: Language,
) -> String {
    let args = serde_json::to_string_pretty(arguments).unwrap_or_default();
    let mut details = format!(
        "{}: {}\n\n{}\n{}",
        language.text("Tool", "工具"),
        display_text(name),
        language.text("Arguments", "参数"),
        display_text(&args)
    );
    if let Some(plan) = plan {
        let formatted = serde_json::to_string_pretty(plan).unwrap_or_default();
        let _ = write!(
            details,
            "\n\n{}\n{}",
            language.text("Execution details", "执行详情"),
            display_text(&formatted)
        );
    }
    details
}

fn tool_details(
    call: &fliuno_protocol::ToolCall,
    plan: Option<&Value>,
    language: Language,
) -> String {
    format_details(&call.name, &call.arguments, plan, language)
}

pub(super) fn review(
    task: &str,
    id: &str,
    root: &str,
    details: ApprovalDetails,
    language: Language,
    remember: bool,
) -> Option<Review> {
    match details {
        ApprovalDetails::ToolCall { approval } => {
            if approval.task_id != task || approval.id != id {
                return None;
            }
            if let Some(plan) = approval.execution_plan.as_ref()
                && let Some(text) = project_check_review(root, plan, language)
            {
                return Some(Review::new(
                    text,
                    tool_details(&approval.call, Some(plan), language),
                    true,
                ));
            }
            let call = &approval.call;
            if call.name == "workspace_edit_text" {
                let path = display_text(call.arguments["path"].as_str()?);
                let base = call.arguments["baseSha256"].as_str()?;
                let before = exact_edit_text(call.arguments["oldText"].as_str()?);
                let replacement = call.arguments["newText"].as_str()?;
                let after = if replacement.is_empty() {
                    language.text("(remove text)", "（移除文本）").into()
                } else {
                    exact_edit_text(replacement)
                };
                let heading = format!(
                    "{}\n{}: {path}\n\n",
                    language.text("Replace an exact text match", "替换精确匹配的文本"),
                    language.text("Path", "路径")
                );
                return Some(
                    Review::new(
                        format!(
                            "{heading}{}\n{before}\n\n{}\n{after}\n\n{}\n\n{}: {root}\n{}: {base}",
                            language.text("Before", "替换前"),
                            language.text("After", "替换后"),
                            language.text(
                                "Requires one match and an unchanged file.",
                                "要求原文只匹配一次，且文件尚未变更。"
                            ),
                            language.text("Workspace", "工作区"),
                            language.text("Base SHA-256", "基准 SHA-256"),
                        ),
                        {
                            let mut arguments = call.arguments.clone();
                            if let Some(fields) = arguments.as_object_mut() {
                                fields.remove("oldText");
                                fields.remove("newText");
                            }
                            format_details(
                                &call.name,
                                &arguments,
                                approval.execution_plan.as_ref(),
                                language,
                            )
                        },
                        true,
                    )
                    .verbatim_from(heading.len()),
                );
            }
            let (action, consequence, preview_key) = match call.name.as_str() {
                "artifact_create" => (
                    language.text("Save a text deliverable", "保存文字交付物"),
                    language.text("Creates an item in Deliverables. No workspace file is written.", "在交付物中保存一项，不会写入工作区文件。"),
                    Some(("content", language.text("Text to save", "将要保存的文字"))),
                ),
                "artifact_create_binary" => (
                    language.text("Save a downloadable deliverable", "保存可下载交付物"),
                    language.text("Creates an item in Deliverables. No workspace file is written.", "在交付物中保存一项，不会写入工作区文件。"),
                    None,
                ),
                "workspace_create_text" => (
                    language.text("Create a workspace file", "创建工作区文件"),
                    language.text("Writes a new file at the requested path. Existing files are not overwritten.", "在指定路径新建文件，不会覆盖已有文件。"),
                    Some(("content", language.text("Text to write", "将要写入的文字"))),
                ),
                "workspace_patch_text" => (
                    language.text("Apply a patch to a workspace file", "为工作区文件应用补丁"),
                    language.text("Changes the file only if its saved contents still match the reviewed base.", "仅在文件内容仍与审阅时的基准一致时应用。"),
                    Some(("patch", language.text("Patch to apply", "将要应用的补丁"))),
                ),
                "workspace_replace_text" => (
                    language.text("Write a workspace file", "写入工作区文件"),
                    language.text("Creates or replaces the specified file. An existing file must match the reviewed base.", "新建或替换指定文件。已有文件必须仍与审阅时的基准一致。"),
                    Some(("content", language.text("Text to write", "将要写入的文字"))),
                ),
                "workspace_find" => (
                    language.text("Search project files", "搜索项目文件"),
                    language.text("Searches the selected directory for matching names or text. No file is changed.", "在指定目录匹配文件名或内容，不会改动文件。"),
                    None,
                ),
                "workspace_read_text" | "workspace_read_bytes" => (
                    language.text("Read a workspace file", "读取工作区文件"),
                    language.text("Sends the requested file content to the model. No file is changed.", "将请求的文件内容发送给模型，不会改动文件。"),
                    None,
                ),
                _ => (
                    language.text("Review this operation", "审阅此次操作"),
                    language.text("Check the target and arguments before allowing it.", "允许前请核对目标和参数。"),
                    None,
                ),
            };
            let mut arguments = call.arguments.clone();
            let preview = preview_key.and_then(|(key, label)| {
                arguments
                    .as_object_mut()?
                    .remove(key)
                    .map(|value| (label, value))
            });
            let identity = call.arguments["path"]
                .as_str()
                .map(|value| (language.text("Path", "路径"), value))
                .or_else(|| {
                    call.arguments["title"]
                        .as_str()
                        .map(|value| (language.text("Title", "标题"), value))
                });
            let mut text = format!(
                "{action}\n{consequence}\n\n{}: {root}\n",
                language.text("Workspace", "工作区")
            );
            if let Some((kind, value)) = identity {
                text.push_str(&format!(
                    "{kind}: {}\n",
                    display_text(value).replace('\n', "\\n")
                ));
            }
            text.push_str(language.text(
                "\nAllow once runs this request. Deny stops it without running the operation.\n",
                "\n允许一次会执行此请求。拒绝后不执行。\n",
            ));
            let prose_end = preview.as_ref().map_or(usize::MAX, |_| text.len());
            if let Some((label, value)) = preview {
                text.push_str(&format!(
                    "\n{label}\n{}\n",
                    display_text(
                        value
                            .as_str()
                            .unwrap_or(language.text("(invalid text)", "（无效文本）"))
                    )
                ));
            }
            if remember {
                text.push_str(language.text(
                    "\nRemember the same path and read options\nin this conversation for 1 hour.\nRevoke with /permissions.\n",
                    "\n在此对话中记住相同路径和读取参数一小时。\n可用 /permissions 撤销。\n",
                ));
            }
            let details = format_details(
                &call.name,
                &arguments,
                approval.execution_plan.as_ref(),
                language,
            );
            Some(Review::new(text, details, true).verbatim_from(prose_end))
        }
        ApprovalDetails::AgentPermission { ref approval }
        | ApprovalDetails::HostTool { ref approval } => {
            if approval.id != id {
                return None;
            }
            let label = match details {
                ApprovalDetails::HostTool { .. } => {
                    language.text("Agent host tool", "Agent 宿主工具")
                }
                _ => language.text("External Agent permission", "外部 Agent 权限"),
            };
            let request =
                display_text(&serde_json::to_string_pretty(&approval.request).unwrap_or_default());
            let heading = format!(
                "{label}\n{}: {root}\n{}: {}\n\n",
                language.text("Workspace", "工作区"),
                language.text("Run", "运行标识"),
                display_text(&approval.run_id),
            );
            Some(
                Review::new(
                    format!("{heading}{request}"),
                    String::new(),
                    approval.allow_once_available,
                )
                .verbatim_from(heading.len()),
            )
        }
    }
}

fn project_check_review(
    root: &str,
    plan: &serde_json::Value,
    language: Language,
) -> Option<String> {
    if plan["kind"] != "project_check" {
        return None;
    }
    let label = display_text(plan["label"].as_str()?);
    let command = display_text(&project_check_command(plan)?);
    let inputs = plan["inputs"].as_object()?;
    let isolation = match plan["isolation"].as_str()? {
        "windows_app_container" => "Windows AppContainer",
        "linux_landlock" => "Linux Landlock",
        _ => return None,
    };
    let mut text = format!(
        "{}: {label}\n\n{}: {command}\n{}\n{}: {isolation}\n\n{} {root}\n",
        language.text("Run project check", "运行项目检查"),
        language.text("Command", "命令"),
        language.text("Run from: private snapshot", "运行位置：私有快照"),
        language.text("Isolation", "隔离方式"),
        language.text("Copy from", "复制自"),
    );
    for path in inputs.keys() {
        text.push_str(&format!("  {}\n", display_text(path)));
    }
    text.push_str(language.text("\nOnly listed copies are checked. Changes in the scratch directory are not written back. Deny skips the check.", "\n只检查列出的副本。临时目录中的更改不会写回工作区。拒绝会跳过此次检查。"));
    Some(text)
}
