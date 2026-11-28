use fliuno_protocol::TaskPlanStepStatus;

use super::{
    transcript::{message_lines, reply_label, response_lines},
    *,
};

impl App {
    pub fn draw(&self, frame: &mut Frame<'_>) {
        let viewport = frame.area();
        frame.render_widget(Block::default().style(self.palette.base), viewport);
        let width = viewport.width.min(112);
        let area = Rect::new(
            viewport.x + (viewport.width - width) / 2,
            viewport.y,
            width,
            viewport.height,
        );
        let editing = matches!(self.layer, Layer::Edit(Edit::Task));
        let full_page = matches!(
            self.layer,
            Layer::Inbox(_) | Layer::History(_) | Layer::TaskBrowser(_)
        );
        let show_composer = !full_page && self.session.is_some();
        let has_notice = self.busy || !self.message.is_empty();
        let [header, body, composer, status_line] = Layout::vertical([
            Constraint::Length(if area.height >= 10 { 3 } else { 1 }),
            Constraint::Min(0),
            Constraint::Length(if !show_composer {
                0
            } else if editing && !self.demo {
                let max_rows = (area.height / 4).clamp(3, 8) - 2;
                let (lines, _, _) = self.draft.viewport(area.width.saturating_sub(4), max_rows);
                lines.len() as u16 + 2
            } else if editing {
                (area.height / 4).clamp(3, 8)
            } else {
                3
            }),
            Constraint::Length(1 + u16::from(has_notice && area.height >= 12)),
        ])
        .areas(area);
        self.draw_header(frame, header);
        let overlay = show_composer
            && body.height >= 7
            && match &self.layer {
                Layer::Commands(menu) => menu.origin != CommandOrigin::Workbench,
                Layer::ModelPicker(_) | Layer::Language(_) => true,
                _ => false,
            };
        if !full_page && !overlay && !matches!(self.layer, Layer::None | Layer::Edit(Edit::Task)) {
            self.draw_layer(
                frame,
                Rect::new(
                    area.x,
                    body.y,
                    area.width,
                    area.bottom().saturating_sub(body.y),
                ),
            );
            return;
        }
        match &self.layer {
            Layer::Inbox(_) => self.draw_inbox(frame, body),
            Layer::History(_) => self.draw_history(frame, body),
            Layer::TaskBrowser(browser) => self.draw_task_browser(frame, body, browser),
            _ => self.draw_conversation(
                frame,
                body.inner(Margin::new(u16::from(body.width >= 24) * 2, 0)),
            ),
        }
        if show_composer {
            self.draw_composer(frame, composer, editing);
        }
        self.draw_status_line(frame, status_line, editing, full_page);
        if overlay {
            self.draw_layer(frame, body);
        }
    }

    fn draw_header(&self, frame: &mut Frame<'_>, area: Rect) {
        let session = match self.layer {
            Layer::Inbox(_) => self.language.text("Workspace inbox", "工作区待办").into(),
            Layer::History(_) => self.language.text("Workspace history", "工作区历史").into(),
            _ => self
                .session
                .as_ref()
                .map(session_name)
                .unwrap_or_else(|| self.language.text("No session", "暂无会话").into()),
        };
        let area = area.inner(Margin::new(u16::from(area.width >= 24) * 2, 0));
        let state = if !self.online {
            self.language.text("Offline", "离线")
        } else if self.demo {
            self.language.text("Offline demo", "离线演示")
        } else {
            self.model
                .as_deref()
                .unwrap_or(self.language.text("Connected", "已连接"))
        };
        let project = if !self.demo && !matches!(self.layer, Layer::Inbox(_) | Layer::History(_)) {
            self.workspace
                .as_ref()
                .map(|root| workspace_title(&root.root))
        } else {
            None
        };
        let title = project.as_deref().unwrap_or(&session);
        let title_room = usize::from(area.width).saturating_sub("Fliuno  /  ".width());
        let state_separator = if project.is_some() && area.width < 58 {
            " · "
        } else {
            "  ·  "
        };
        let state_width = state
            .width()
            .min(title_room.saturating_sub(title.width().min(12) + state_separator.width()));
        let state_suffix = if state_width >= 2 {
            format!("{state_separator}{}", ellipsis(state, state_width))
        } else {
            String::new()
        };
        let title_width = title_room.saturating_sub(state_suffix.width());
        let root = self
            .workspace
            .as_ref()
            .map_or_else(String::new, |root| display_path(&root.root));
        let breadcrumb = self
            .session
            .as_ref()
            .and_then(|session| session.fork_origin.as_ref())
            .map_or(root.clone(), |origin| {
                format!("{} inherited turns · {root}", origin.inherited_turns)
            });
        let room = usize::from(area.width);
        let location = if project.is_some() && !session.is_empty() && room >= 65 {
            let session = ellipsis(&session.replace('\n', " "), room / 3);
            let path = path_tail(&breadcrumb, room.saturating_sub(session.width() + 3));
            format!("{session} · {path}")
        } else {
            path_tail(&breadcrumb, room)
        };
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(vec![
                    Span::styled("Fliuno", self.palette.label),
                    Span::raw(format!("  /  {}", ellipsis(title, title_width))),
                    Span::styled(state_suffix, self.palette.quiet),
                ]),
                Line::styled(location, self.palette.quiet),
            ]),
            area,
        );
    }

    fn draw_status_line(&self, frame: &mut Frame<'_>, area: Rect, editing: bool, full_page: bool) {
        let message = if self.deferred_send.is_some() {
            self.language
                .text("Will send when conversation opens", "会话加载后将发送")
        } else if self.reopening.is_some() {
            self.language.text("Opening conversation…", "正在打开会话…")
        } else if self.busy {
            self.language.text("Working", "处理中")
        } else {
            &self.message
        };
        let help = if self.deferred_send.is_some() {
            self.language.text("Esc Cancel send", "Esc 取消发送")
        } else if matches!(self.layer, Layer::Language(_))
            || matches!(&self.layer, Layer::ModelPicker(picker) if picker.editing_suppliers())
        {
            ""
        } else if editing {
            if !self.demo && area.width < 60 {
                if self.draft.text().is_empty() {
                    self.language.text(
                        "Enter Send  Ctrl+N Line  / Menu  Esc Back",
                        "Enter 发送 · Ctrl+N 换行 · / 命令 · Esc 返回",
                    )
                } else {
                    self.language.text(
                        "Enter Send  Ctrl+N Line  F1 Menu  Esc Back",
                        "Enter 发送 · Ctrl+N 换行 · F1 命令 · Esc 返回",
                    )
                }
            } else if !self.demo {
                if self.draft.text().is_empty() {
                    self.language.text(
                        "Enter Send  Ctrl+N New line  / Commands  Esc Back",
                        "Enter 发送 · Ctrl+N 换行 · / 命令 · Esc 返回",
                    )
                } else {
                    self.language.text(
                        "Enter Send  Ctrl+N New line  F1 Commands  Esc Back",
                        "Enter 发送 · Ctrl+N 换行 · F1 命令 · Esc 返回",
                    )
                }
            } else if area.width < 60 {
                "Ctrl+S Send  Ctrl+E Queue  F2 Mode  F3 Context"
            } else {
                "Ctrl+S Send  Ctrl+E Queue  F2 Mode  F3 Context  Esc Back"
            }
        } else if full_page {
            self.language
                .text("Enter Open  Esc Back", "Enter 打开 · Esc 返回")
        } else if matches!(self.layer, Layer::None) {
            if !self.demo && !self.can_compose() {
                self.language.text(
                    "Enter Review  F1 Commands  Tab Details",
                    "Enter 审阅 · F1 命令 · Tab 详情",
                )
            } else if !self.demo && !self.show_details && self.transcript.has_older() {
                self.language.text(
                    "Enter Write  PgUp History  F1 Commands",
                    "Enter 输入 · PgUp 历史 · F1 命令",
                )
            } else if !self.demo {
                self.language.text(
                    "Enter Write  F1 Commands  Tab Details",
                    "Enter 输入 · F1 命令 · Tab 详情",
                )
            } else if area.width < 66 {
                "F1 Menu  n New  Tab Details  Shift+L Tasks"
            } else {
                "F1 Commands  n New  Tab Details  Shift+L All tasks"
            }
        } else {
            self.language.text("Esc Back", "Esc 返回")
        };
        let area = area.inner(Margin::new(u16::from(area.width >= 24) * 2, 0));
        let width = usize::from(area.width);
        let mut lines = vec![Line::styled(ellipsis(help, width), self.palette.quiet)];
        if !message.is_empty() && area.height >= 2 {
            let notice = clean(message).replace('\n', " ");
            lines.push(Line::styled(ellipsis(&notice, width), self.palette.label));
        } else if self.busy && area.height == 1 {
            lines[0] = Line::styled(ellipsis(message, width), self.palette.label);
        }
        frame.render_widget(Paragraph::new(lines), area);
    }

    fn draw_conversation(&self, frame: &mut Frame<'_>, area: Rect) {
        let (history, active_rendered) = if self.demo || self.show_details {
            (transcript::LayoutAtWidth::default(), false)
        } else {
            self.transcript.layout(
                &self.palette,
                area.width,
                self.selected.as_deref(),
                self.language,
            )
        };
        let saved_rows = history.row_count();
        // A placeholder lets the live tail use the same spacing as if saved
        // rows were already in this buffer. Remove it before measuring.
        let mut lines = Vec::new();
        if saved_rows > 0 {
            lines.push(Line::default());
        }
        let mut pinned = Vec::new();
        if let Some(detail) = &self.detail {
            let task = &detail.task;
            if self.show_details {
                pinned.push(Line::styled(
                    self.language.text("Task detail", "任务详情"),
                    self.palette.label,
                ));
            }
            let (label, color) = status(task.status);
            let mut heading = vec![Span::styled(label, self.palette.status(color))];
            if task
                .context
                .as_ref()
                .is_some_and(|context| context.options.plan_only)
            {
                heading.push(Span::styled(
                    self.language.text("  ·  Plan only", "  ·  仅规划"),
                    self.palette.quiet,
                ));
            }
            if self.demo || self.show_details || task.status != TaskStatus::Completed {
                if self.follow && area.width >= 48 {
                    heading.push(Span::styled(
                        self.language.text("  ·  Following", "  ·  跟随输出"),
                        self.palette.quiet,
                    ));
                }
                pinned.push(Line::from(heading));
            }
            self.append_task_actions(detail, &mut pinned);
            if !self.show_details
                && !task.status.is_terminal()
                && let Some(plan) = &detail.plan
            {
                let done = plan
                    .steps
                    .iter()
                    .filter(|step| step.status == TaskPlanStepStatus::Completed)
                    .count();
                let progress = format!("Plan · {done}/{} complete", plan.steps.len());
                let width = usize::from(area.width);
                let hint = if width
                    >= progress.width()
                        + self
                            .language
                            .text("  ·  Tab Details", "  ·  Tab 详情")
                            .width()
                    && !self.demo
                {
                    self.language.text("  ·  Tab Details", "  ·  Tab 详情")
                } else {
                    ""
                };
                pinned.push(Line::styled(
                    ellipsis(&format!("{progress}{hint}"), width),
                    self.palette.label,
                ));
                if let Some(step) = plan
                    .steps
                    .iter()
                    .find(|step| step.status == TaskPlanStepStatus::InProgress)
                {
                    pinned.push(Line::styled(
                        ellipsis(
                            &format!("Now: {}", clean(&step.text).replace('\n', " ")),
                            width,
                        ),
                        self.palette.quiet,
                    ));
                }
            }
            let final_reply = task
                .output
                .as_ref()
                .filter(|_| task.status == TaskStatus::Completed)
                .and_then(|output| {
                    task.messages.iter().rposition(|message| {
                        matches!(message, Message::Assistant { text, .. } if text == output)
                    })
                });
            if !active_rendered {
                for (index, message) in task
                    .messages
                    .iter()
                    .enumerate()
                    .filter(|(_, message)| {
                        matches!(message, Message::User { .. } | Message::Assistant { .. })
                    })
                    .take(16)
                {
                    match message {
                        Message::User { text } => message_lines(
                            &mut lines,
                            self.language.text("You", "你"),
                            text,
                            self.palette.label,
                        ),
                        Message::Assistant { text, identity } if final_reply != Some(index) => {
                            response_lines(
                                &mut lines,
                                reply_label(identity.as_ref(), area.width, self.language),
                                text,
                                &self.palette,
                                area.width,
                            );
                        }
                        _ => {}
                    }
                }
            }
            if task.status == TaskStatus::Running {
                let phase = if let Some(call) = &task.in_flight_tool {
                    project_activity::describe(call, None, self.language).map_or_else(
                        || format!("Running tool  ·  {}", clean(&call.name)),
                        |action| format!("Running  ·  {action}"),
                    )
                } else if task.agent_run.is_some() {
                    self.language
                        .text("External Agent running", "外部 Agent 正在运行")
                        .into()
                } else if task.model_steps > 0 {
                    let limit = task
                        .max_model_steps
                        .unwrap_or(fliuno_protocol::DEFAULT_TASK_MODEL_STEPS);
                    format!("Model request  ·  Attempt {}/{limit}", task.model_steps)
                } else {
                    self.language
                        .text("Waiting for a model worker", "等待模型开始运行")
                        .into()
                };
                pinned.push(Line::styled(phase, self.palette.quiet));
                if task.in_flight_tool.is_none()
                    && let Some(preview) = self.activity.iter().find(|preview| {
                        preview.task_id == task.id
                            && preview.step == task.model_steps
                            && !preview.text.is_empty()
                    })
                {
                    let title = reply_label(
                        task.active_reply_identity.as_ref(),
                        area.width,
                        self.language,
                    );
                    response_lines(&mut lines, title, &preview.text, &self.palette, area.width);
                    if preview.clipped {
                        lines.push(Line::styled(
                            self.language.text("Preview limit reached", "预览达到上限"),
                            self.palette.quiet,
                        ));
                    }
                }
            }
            if !active_rendered && let Some(output) = &task.output {
                if task.status == TaskStatus::Failed {
                    message_lines(
                        &mut lines,
                        self.language.text("Failure reason", "失败原因"),
                        output,
                        self.palette.label,
                    );
                } else {
                    let identity = final_reply.and_then(|index| match &task.messages[index] {
                        Message::Assistant { identity, .. } => identity.as_ref(),
                        _ => None,
                    });
                    response_lines(
                        &mut lines,
                        reply_label(identity, area.width, self.language),
                        output,
                        &self.palette,
                        area.width,
                    );
                }
            }
            if let Some(usage) = &detail.usage {
                lines.push(Line::raw(""));
                lines.push(Line::styled(
                    format!(
                        "Tokens  ·  {} input  ·  {} output",
                        usage.input_tokens.0, usage.output_tokens.0
                    ),
                    self.palette.quiet,
                ));
            }
            if self.show_details {
                self.append_task_details(detail, &mut lines);
            } else if !detail.artifacts.is_empty() {
                lines.push(Line::raw(""));
                let title = if detail.artifacts.len() == 1 {
                    clean(&detail.artifacts[0].title).replace('\n', " ")
                } else {
                    format!("{} items", detail.artifacts.len())
                };
                lines.push(Line::styled(
                    format!("Saved in Deliverables: {title}  ·  v Open / export"),
                    self.palette.quiet,
                ));
            }
            self.append_project_activity(task, &mut lines, area.width);
        } else if lines.is_empty() {
            let (title, hint) = if !self.online {
                if self.busy {
                    (self.language.text("Connecting", "正在连接"), "")
                } else {
                    (
                        self.language.text("Disconnected", "连接已断开"),
                        self.language.text(
                            "r Reconnect  ·  F5 Check configuration",
                            "r 重连 · F5 检查配置",
                        ),
                    )
                }
            } else if self.selected.is_some() || self.snapshot.is_none() {
                (self.language.text("Loading session", "正在加载会话"), "")
            } else if self.demo {
                (self.language.text("No tasks yet", "暂无任务"), "")
            } else {
                (
                    self.language.text("Work in this folder", "在此目录中工作"),
                    "",
                )
            };
            lines.push(Line::styled(title, self.palette.label));
            if !hint.is_empty() {
                lines.push(Line::styled(hint, self.palette.quiet));
            } else if self.online
                && !self.demo
                && self.selected.is_none()
                && self.snapshot.is_some()
            {
                lines.push(Line::raw(""));
                for (label, example) in [
                    (
                        self.language.text("Explore", "探索"),
                        self.language.text(
                            "Find where a feature is implemented.",
                            "查找功能的实现位置。",
                        ),
                    ),
                    (
                        self.language.text("Change", "修改"),
                        self.language
                            .text("Describe the fix you want.", "描述你想修复的问题。"),
                    ),
                    (
                        self.language.text("Verify", "验证"),
                        self.language
                            .text("Run a configured project check.", "运行已配置的项目检查。"),
                    ),
                ] {
                    lines.push(Line::from(vec![
                        Span::styled(
                            format!(
                                "{label}{}",
                                " ".repeat(8usize.saturating_sub(label.width()))
                            ),
                            self.palette.label,
                        ),
                        Span::styled(example, self.palette.quiet),
                    ]));
                }
                lines.push(Line::default());
                lines.push(Line::styled(
                    self.language
                        .text("F2 Plan mode  ·  F1 Commands", "F2 规划模式 · F1 命令"),
                    self.palette.quiet,
                ));
            }
        }
        let header = Paragraph::new(pinned).wrap(Wrap { trim: false });
        let header_height = header
            .line_count(area.width)
            .min(usize::from(area.height / 2)) as u16;
        let [heading, conversation] =
            Layout::vertical([Constraint::Length(header_height), Constraint::Min(0)]).areas(area);
        frame.render_widget(header, heading);
        let tail = &lines[usize::from(saved_rows > 0)..];
        let tail_rows = Paragraph::new(tail.to_vec())
            .wrap(Wrap { trim: false })
            .line_count(conversation.width);
        let height = usize::from(conversation.height);
        let max_scroll = (saved_rows + tail_rows).saturating_sub(height);
        let saved = self
            .scroll_anchor
            .get()
            .and_then(|anchor| history.resolve(anchor));
        let scroll = if self.follow {
            self.scroll_anchor.set(None);
            max_scroll
        } else {
            let row = saved.unwrap_or_else(|| self.scroll.get()).min(max_scroll);
            if saved.is_none() {
                self.scroll_anchor.set(history.anchor_at(row));
            }
            row
        };
        self.rendered_scroll.set(scroll);
        if conversation.is_empty() {
            return;
        }
        let end = scroll + height;
        let (mut visible, mut skip) = history.visible_lines(scroll, end);
        if end > saved_rows {
            visible.extend_from_slice(tail);
            if scroll >= saved_rows {
                skip = scroll - saved_rows;
            }
        }
        let skip = skip.min(usize::from(u16::MAX)) as u16;
        frame.render_widget(
            Paragraph::new(visible)
                .wrap(Wrap { trim: false })
                .scroll((skip, 0)),
            conversation,
        );
    }

    fn append_task_actions(&self, detail: &Detail, lines: &mut Vec<Line<'static>>) {
        let task = &detail.task;
        let action = if let Some(approval) = &task.pending_approval {
            let action = project_activity::describe(
                &approval.call,
                approval.execution_plan.as_ref(),
                self.language,
            )
            .unwrap_or_else(|| clean(&approval.call.name));
            Some(format!("Review operation  ·  a Open\n{action}"))
        } else if pending_approval_id(task).is_some() {
            Some("Review operation  ·  a Open".into())
        } else if let Some(question) = task
            .pending_tool_input
            .as_ref()
            .and_then(|pending| pending.requests["question"].as_str())
        {
            Some(format!(
                "Clarification requested  ·  a Answer\n{}",
                clean(question)
            ))
        } else if let Some(review) = &detail.review {
            Some(format!(
                "External outcome recorded: {}  ·  r Review",
                outcome_label(review.review.state)
            ))
        } else if detail.can_reconcile {
            Some("External result needs verification  ·  r Review".into())
        } else if detail.resume.is_some() {
            Some("Task interrupted  ·  r Review recovery".into())
        } else {
            self.snapshot
                .as_ref()
                .and_then(|snap| snap.next_actions.as_ref())
                .and_then(|actions| actions.iter().find(|action| action.task_id == task.id))
                .map(|action| {
                    match action.kind {
                        WorkActionKind::ReviewApproval => "Review operation  ·  a Open",
                        WorkActionKind::AnswerInput => "Clarification requested  ·  a Answer",
                        WorkActionKind::Recover { .. } => "Recovery available  ·  r Review",
                        WorkActionKind::Reconcile => {
                            "External result needs verification  ·  r Review"
                        }
                        WorkActionKind::ReviewBudget { .. } => {
                            "Context budget reached  ·  c Review"
                        }
                    }
                    .into()
                })
        };
        if let Some(action) = action {
            lines.extend(
                action
                    .lines()
                    .map(|line| Line::styled(line.to_owned(), self.palette.status(Color::Yellow))),
            );
        }
    }

    fn append_task_details(&self, detail: &Detail, lines: &mut Vec<Line<'static>>) {
        if let Some(resume) = &detail.resume {
            lines.push(Line::raw(""));
            lines.push(Line::styled(
                disposition_text(resume.disposition),
                self.palette.quiet,
            ));
        }
        if let Some(context) = &detail.task.context {
            let options = &context.options;
            let setting = if options.mode == ContextMode::Isolated {
                "Isolated context".into()
            } else {
                let history = if options.detail == ContextDetail::Conversation {
                    "Conversation only"
                } else {
                    "Full history"
                };
                let memory = if options.disable_memory {
                    "Memory off".into()
                } else if let Some(picks) = &options.memory_selection {
                    format!("{} pinned memories", picks.len())
                } else {
                    "Memory automatic".into()
                };
                format!(
                    "{history} · {memory} · through #{}",
                    context.through_sequence
                )
            };
            lines.push(Line::styled(setting, self.palette.quiet));
        }
        if let Some(plan) = &detail.plan {
            lines.push(Line::raw(""));
            lines.push(Line::styled("Plan", self.palette.label));
            for step in &plan.steps {
                let state = match step.status {
                    TaskPlanStepStatus::Pending => "Next",
                    TaskPlanStepStatus::InProgress => "In progress",
                    TaskPlanStepStatus::Completed => "Done",
                };
                lines.push(Line::raw(format!("{state}  ·  {}", clean(&step.text))));
            }
        }
        if !detail.artifacts.is_empty() {
            lines.push(Line::raw(""));
            lines.push(Line::styled("Artifacts  ·  v Open", self.palette.label));
            for artifact in &detail.artifacts {
                lines.push(Line::raw(format!(
                    "{}  ·  {} B",
                    clean(&artifact.title),
                    artifact.content_bytes
                )));
            }
        }
    }
    fn draw_composer(&self, frame: &mut Frame<'_>, area: Rect, focused: bool) {
        if area.width < 4 || area.height == 0 {
            return;
        }
        let mode = if self.plan_only {
            self.language.text("Plan only", "仅规划")
        } else if !self.demo {
            self.language.text("Project prompt", "项目输入")
        } else if focused {
            self.language.text("New task · Execution", "新任务 · 执行")
        } else {
            self.language.text("Draft · Execution", "草稿 · 执行")
        };
        let title = if self.session.is_none() {
            self.language.text(" No session ", " 暂无会话 ").to_owned()
        } else {
            let mut title = format!(" {mode} ");
            match self.draft_context.tool_scope {
                ToolScope::All => {}
                ToolScope::Project => {
                    title.push_str(self.language.text("· Project tools ", "· 项目工具 "))
                }
                ToolScope::None => title.push_str(self.language.text("· No tools ", "· 无工具 ")),
            }
            if self.draft_context.mode == ContextMode::Isolated {
                title.push_str(self.language.text("· Isolated ", "· 隔离上下文 "));
            } else if self.draft_context.through_sequence.is_some() {
                title.push_str(self.language.text("· History pinned ", "· 历史已固定 "));
            } else if self.draft_context.disable_memory {
                title.push_str(self.language.text("· Memory off ", "· 已禁用记忆 "));
            }
            if self.enabled_project_guide().is_some()
                && self.draft_context.mode == ContextMode::Session
            {
                title.push_str(self.language.text("· AGENTS.md ", "· 已选 AGENTS.md "));
            }
            if self.input_artifact.is_some() {
                title.push_str(self.language.text(" · Attachment ", " · 附件 "));
            }
            title
        };
        let block = Block::default()
            .borders(Borders::TOP)
            .style(self.palette.rail)
            .border_style(self.palette.quiet)
            .title(Span::styled(title, self.palette.label));
        let body = block.inner(area).inner(Margin::new(2, 0));
        frame.render_widget(block, area);
        if body.height == 0 || body.width == 0 {
            return;
        }
        if let Layer::Commands(menu) = &self.layer
            && menu.origin == CommandOrigin::Slash
        {
            let command = format!("/{}", menu.query);
            let shown = ellipsis(&command, usize::from(body.width));
            let caret = shown.width().min(usize::from(body.width - 1)) as u16;
            frame.render_widget(Paragraph::new(shown), body);
            frame.set_cursor_position(Position::new(body.x + caret, body.y));
            return;
        }
        let (lines, x, y) = self.draft.viewport(body.width, body.height);
        if self.draft.text().is_empty() && (!focused || !self.demo) {
            if self.session.is_some() {
                frame.render_widget(
                    Paragraph::new(if self.demo {
                        self.language
                            .text("n or Enter to write a task", "按 n 或 Enter 输入任务")
                    } else if !focused && !self.can_compose() {
                        self.language.text(
                            "Review the pending operation to continue",
                            "审阅待批准的操作后继续",
                        )
                    } else if focused && self.detail.is_some() {
                        self.language.text("Ask a follow-up…", "继续追问…")
                    } else if focused {
                        self.language.text(
                            "Describe a change or ask about the project…",
                            "描述要做的改动，或询问项目…",
                        )
                    } else {
                        self.language.text(
                            "Press Enter to enter a project prompt",
                            "按 Enter 输入项目请求",
                        )
                    })
                    .style(self.palette.quiet),
                    body,
                );
            }
        } else {
            frame.render_widget(
                Paragraph::new(lines.into_iter().map(Line::raw).collect::<Vec<_>>()),
                body,
            );
        }
        if focused && !self.start_pending {
            frame.set_cursor_position(Position::new(body.x + x, body.y + y));
        }
    }
}

fn workspace_title(root: &str) -> String {
    let path = display_path(root).replace('\n', " ");
    path.trim_end_matches(['/', '\\'])
        .rsplit(['/', '\\'])
        .find(|part| !part.is_empty())
        .map_or_else(|| path.clone(), str::to_owned)
}
