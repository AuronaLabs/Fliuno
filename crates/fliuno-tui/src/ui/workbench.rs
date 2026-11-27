use super::{
    transcript::{message_lines, response_lines},
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
                Layer::ModelPicker(_) => true,
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
            Layer::Inbox(_) => "Workspace inbox".into(),
            Layer::History(_) => "Workspace history".into(),
            _ => self
                .session
                .as_ref()
                .map(session_name)
                .unwrap_or_else(|| "No session".into()),
        };
        let area = area.inner(Margin::new(u16::from(area.width >= 24) * 2, 0));
        let state = if !self.online {
            "Offline"
        } else if self.demo {
            "Offline demo"
        } else {
            self.model.as_deref().unwrap_or("Connected")
        };
        let title_room = usize::from(area.width).saturating_sub("Fliuno  /  ".width());
        let state_separator = "  ·  ";
        let state_width = state
            .width()
            .min(title_room.saturating_sub(session.width().min(12) + state_separator.width()));
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
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(vec![
                    Span::styled("Fliuno", self.palette.label),
                    Span::raw(format!("  /  {}", ellipsis(&session, title_width))),
                    Span::styled(state_suffix, self.palette.quiet),
                ]),
                Line::styled(
                    path_tail(&breadcrumb, usize::from(area.width)),
                    self.palette.quiet,
                ),
            ]),
            area,
        );
    }

    fn draw_status_line(&self, frame: &mut Frame<'_>, area: Rect, editing: bool, full_page: bool) {
        let message = if self.busy { "Working" } else { &self.message };
        let help = if editing {
            if !self.demo && area.width < 60 {
                if self.draft.text().is_empty() {
                    "Enter Send  Ctrl+N Line  / Menu  Esc Back"
                } else {
                    "Enter Send  Ctrl+N Line  F1 Menu  Esc Back"
                }
            } else if !self.demo {
                if self.draft.text().is_empty() {
                    "Enter Send  Ctrl+N New line  / Commands  Esc Back"
                } else {
                    "Enter Send  Ctrl+N New line  F1 Commands  Esc Back"
                }
            } else if area.width < 60 {
                "Ctrl+S Send  Ctrl+E Queue  F2 Mode  F3 Context"
            } else {
                "Ctrl+S Send  Ctrl+E Queue  F2 Mode  F3 Context  Esc Back"
            }
        } else if full_page {
            "Enter Open  Esc Back"
        } else if matches!(self.layer, Layer::None) {
            if !self.demo && !self.show_details && self.transcript.has_older() {
                "Enter Write  PgUp History  F1 Commands"
            } else if !self.demo {
                "Enter Write  F1 Commands  Tab Details"
            } else if area.width < 66 {
                "F1 Menu  n New  Tab Details  Shift+L Tasks"
            } else {
                "F1 Commands  n New  Tab Details  Shift+L All tasks"
            }
        } else {
            "Esc Back"
        };
        let area = area.inner(Margin::new(u16::from(area.width >= 24) * 2, 0));
        let width = usize::from(area.width);
        let mut lines = vec![Line::styled(ellipsis(help, width), self.palette.quiet)];
        if !message.is_empty() && area.height >= 2 {
            let notice = clean(message).replace('\n', " ");
            lines.push(Line::styled(ellipsis(&notice, width), self.palette.label));
        } else if self.busy && area.height == 1 {
            lines[0] = Line::styled("Working", self.palette.label);
        }
        frame.render_widget(Paragraph::new(lines), area);
    }

    fn draw_conversation(&self, frame: &mut Frame<'_>, area: Rect) {
        let (mut lines, prefix_lines, active_rendered) = if self.demo || self.show_details {
            (Vec::new(), 0, false)
        } else {
            self.transcript
                .render(&self.palette, area.width, self.selected.as_deref())
        };
        let mut pinned = Vec::new();
        if let Some(detail) = &self.detail {
            let task = &detail.task;
            if self.show_details {
                pinned.push(Line::styled("Task detail", self.palette.label));
            }
            let (label, color) = status(task.status);
            let mut heading = vec![Span::styled(label, self.palette.status(color))];
            if task
                .context
                .as_ref()
                .is_some_and(|context| context.options.plan_only)
            {
                heading.push(Span::styled("  ·  Plan only", self.palette.quiet));
            }
            if self.demo || self.show_details || task.status != TaskStatus::Completed {
                if self.follow && area.width >= 48 {
                    heading.push(Span::styled("  ·  Following", self.palette.quiet));
                }
                pinned.push(Line::from(heading));
            }
            self.append_task_actions(detail, &mut pinned);
            if !active_rendered {
                for message in task
                    .messages
                    .iter()
                    .filter(|message| {
                        matches!(message, Message::User { .. } | Message::Assistant { .. })
                    })
                    .take(16)
                {
                    match message {
                        Message::User { text } => {
                            message_lines(&mut lines, "You", text, self.palette.label)
                        }
                        Message::Assistant { text } if task.output.as_ref() != Some(text) => {
                            response_lines(&mut lines, "Fliuno", text, &self.palette, area.width);
                        }
                        _ => {}
                    }
                }
            }
            if task.status == TaskStatus::Running {
                let phase = if let Some(call) = &task.in_flight_tool {
                    format!("Running tool  ·  {}", clean(&call.name))
                } else if task.agent_run.is_some() {
                    "External Agent running".into()
                } else if task.model_steps > 0 {
                    format!("Model request  ·  Attempt {}", task.model_steps)
                } else {
                    "Waiting for a model worker".into()
                };
                pinned.push(Line::styled(phase, self.palette.quiet));
                if task.in_flight_tool.is_none()
                    && let Some(preview) = self.activity.iter().find(|preview| {
                        preview.task_id == task.id
                            && preview.step == task.model_steps
                            && !preview.text.is_empty()
                    })
                {
                    let title = if self.demo || self.show_details {
                        "Live response"
                    } else {
                        "Fliuno"
                    };
                    response_lines(&mut lines, title, &preview.text, &self.palette, area.width);
                    if preview.clipped {
                        lines.push(Line::styled("Preview limit reached", self.palette.quiet));
                    }
                }
            }
            if !active_rendered && let Some(output) = &task.output {
                if task.status == TaskStatus::Failed {
                    message_lines(&mut lines, "Failure reason", output, self.palette.label);
                } else {
                    response_lines(&mut lines, "Fliuno", output, &self.palette, area.width);
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
                let count = detail.artifacts.len();
                let name = if count == 1 {
                    "deliverable"
                } else {
                    "deliverables"
                };
                lines.push(Line::styled(
                    format!("{count} {name}  ·  v Open"),
                    self.palette.quiet,
                ));
            }
        } else if lines.is_empty() {
            let (title, hint) = if !self.online {
                if self.busy {
                    ("Connecting", "")
                } else {
                    ("Disconnected", "r Reconnect  ·  F5 Check configuration")
                }
            } else if self.selected.is_some() || self.snapshot.is_none() {
                ("Loading session", "")
            } else if self.demo {
                ("No tasks yet", "")
            } else {
                ("Start a conversation", "")
            };
            lines.push(Line::styled(title, self.palette.label));
            if !hint.is_empty() {
                lines.push(Line::styled(hint, self.palette.quiet));
            }
        }
        let header = Paragraph::new(pinned).wrap(Wrap { trim: false });
        let header_height = header
            .line_count(area.width)
            .min(usize::from(area.height / 2)) as u16;
        let [heading, conversation] =
            Layout::vertical([Constraint::Length(header_height), Constraint::Min(0)]).areas(area);
        frame.render_widget(header, heading);
        if prefix_lines > 0 && !self.follow {
            let rows = Paragraph::new(lines[..prefix_lines].to_vec())
                .wrap(Wrap { trim: false })
                .line_count(conversation.width)
                .min(usize::from(u16::MAX)) as u16;
            self.scroll
                .set(self.scroll.get().saturating_add(rows.saturating_add(1)));
        }
        let paragraph = Paragraph::new(lines).wrap(Wrap { trim: false });
        let max_scroll = paragraph
            .line_count(conversation.width)
            .saturating_sub(usize::from(conversation.height))
            .min(usize::from(u16::MAX)) as u16;
        let scroll = if self.follow {
            max_scroll
        } else {
            self.scroll.get().min(max_scroll)
        };
        self.rendered_scroll.set(scroll);
        frame.render_widget(paragraph.scroll((scroll, 0)), conversation);
    }

    fn append_task_actions(&self, detail: &Detail, lines: &mut Vec<Line<'static>>) {
        let task = &detail.task;
        let action = if let Some(approval) = &task.pending_approval {
            Some(format!(
                "Review operation  ·  {}  ·  a Open",
                clean(&approval.call.name)
            ))
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
                lines.push(Line::raw(format!(
                    "{:?}  {}",
                    step.status,
                    clean(&step.text)
                )));
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
            "Plan only"
        } else if !self.demo {
            "Message Fliuno"
        } else if focused {
            "New task · Execution"
        } else {
            "Draft · Execution"
        };
        let title = if self.session.is_none() {
            " No session ".to_owned()
        } else {
            let mut title = format!(" {mode} ");
            if self.draft_context.mode == ContextMode::Isolated {
                title.push_str("· Isolated ");
            } else if self.draft_context.through_sequence.is_some() {
                title.push_str("· History pinned ");
            } else if self.draft_context.disable_memory {
                title.push_str("· Memory off ");
            }
            if self.input_artifact.is_some() {
                title.push_str(" · Attachment ");
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
                        "n or Enter to write a task"
                    } else if focused && self.detail.is_some() {
                        "Ask a follow-up…"
                    } else if focused {
                        "What would you like to do in this folder?"
                    } else {
                        "Press Enter to write a message"
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
