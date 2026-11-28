use super::*;

impl App {
    pub(super) fn draw_task_browser(
        &self,
        frame: &mut Frame<'_>,
        area: Rect,
        browser: &TaskBrowser,
    ) {
        let [title, list] =
            Layout::vertical([Constraint::Length(3), Constraint::Min(0)]).areas(area);
        let filter = browser.status().map_or("All states", |s| status(s).0);
        frame.render_widget(
            Paragraph::new(vec![
                Line::styled(
                    format!(
                        " Session tasks  ·  {filter}  ·  Page {}",
                        browser.cursors.len()
                    ),
                    self.palette.label,
                ),
                Line::styled(
                    " f Filter  PgUp Prev  PgDn Next  r Refresh",
                    self.palette.quiet,
                ),
            ])
            .style(self.palette.rail),
            title,
        );
        let Some(page) = &browser.page else {
            render_text(frame, list, "Loading tasks…", 0);
            return;
        };
        if page.items.is_empty() {
            render_text(frame, list, "No tasks match this filter.", 0);
            return;
        }
        let rows = page
            .items
            .iter()
            .map(|task| {
                let (label, color) = task_status(task);
                let prompt = task_preview(task);
                if area.width < 68 {
                    ListItem::new(vec![
                        Line::styled(label, self.palette.status(color)),
                        Line::raw(prompt),
                    ])
                } else {
                    ListItem::new(Line::from(vec![
                        Span::styled(format!("{label:<19}"), self.palette.status(color)),
                        Span::raw(prompt),
                    ]))
                }
            })
            .collect::<Vec<_>>();
        let mut state = ListState::default().with_selected(Some(browser.index));
        frame.render_stateful_widget(
            List::new(rows)
                .highlight_symbol("› ")
                .highlight_style(self.palette.choice),
            list,
            &mut state,
        );
    }

    pub(super) fn draw_inbox(&self, frame: &mut Frame<'_>, area: Rect) {
        let rows = self.inbox_rows();
        let heading = match self.inbox_filter {
            InboxFilter::Attention => "Needs attention [1]",
            InboxFilter::Active => "Running [2]",
            InboxFilter::Recent => "Recent [3]",
        };
        let [header, list_area] =
            Layout::vertical([Constraint::Length(3), Constraint::Min(0)]).areas(area);
        let counts = self.inbox.as_ref().map(|inbox| &inbox.snapshot.task_counts);
        let mut summary = counts.map_or_else(
            || "Loading workspace…".to_owned(),
            |counts| {
                format!(
                    "{} approvals · {} running · {} waiting input · {} interrupted · {} ended",
                    self.inbox_approvals.len(),
                    counts.running,
                    counts.waiting_input,
                    counts.interrupted,
                    counts
                        .completed
                        .saturating_add(counts.failed)
                        .saturating_add(counts.cancelled),
                )
            },
        );
        if self.inbox_approvals_more {
            summary.push_str(" · more approvals: PgDn at list end");
        }
        frame.render_widget(
            Paragraph::new(vec![
                Line::styled(
                    format!("{heading}    1 Attention / 2 Running / 3 Recent"),
                    self.palette.label,
                ),
                Line::styled(summary, self.palette.quiet),
            ])
            .wrap(Wrap { trim: false }),
            header,
        );
        let items = rows
            .iter()
            .map(|row| {
                let (task, label, session_id) = match row {
                    InboxRow::Approval {
                        task_id,
                        session_id,
                        ..
                    } => {
                        let task = self
                            .inbox
                            .as_ref()
                            .and_then(|inbox| inbox.snapshot.recent_tasks.as_ref())
                            .and_then(|tasks| tasks.iter().find(|task| task.id == *task_id));
                        (task, "Approval", session_id.as_str())
                    }
                    InboxRow::Task(task) => {
                        let label = match task.status {
                            _ if task.model_outcome_unknown || task.agent_outcome_unknown => {
                                "Verify result"
                            }
                            TaskStatus::WaitingInput => "Clarification",
                            TaskStatus::Interrupted => "Resume check",
                            TaskStatus::Failed => "Failed",
                            _ => status(task.status).0,
                        };
                        (Some(task), label, task.session_id.as_str())
                    }
                };
                let title = task
                    .map(|task| clean(&task.input_preview))
                    .unwrap_or_else(|| "Task needs review".to_owned());
                ListItem::new(vec![
                    Line::raw(format!("{label}  ·  {title}")),
                    Line::styled(format!("Session {session_id}"), self.palette.quiet),
                ])
            })
            .collect::<Vec<_>>();
        if items.is_empty() {
            frame.render_widget(
                Paragraph::new(if self.busy {
                    "Refreshing…"
                } else {
                    "Nothing to do in this view."
                })
                .style(self.palette.quiet),
                list_area,
            );
        } else {
            let selected = match self.layer {
                Layer::Inbox(index) => index,
                _ => 0,
            };
            let mut state = ListState::default().with_selected(Some(selected.min(items.len() - 1)));
            frame.render_stateful_widget(
                List::new(items)
                    .highlight_symbol("› ")
                    .highlight_style(self.palette.choice),
                list_area,
                &mut state,
            );
        }
    }

    pub(super) fn draw_history(&self, frame: &mut Frame<'_>, area: Rect) {
        let [header, list_area] =
            Layout::vertical([Constraint::Length(3), Constraint::Min(0)]).areas(area);
        let Some(page) = &self.history_page else {
            frame.render_widget(
                Paragraph::new(if self.busy {
                    "Searching workspace history…"
                } else {
                    "No results loaded"
                }),
                area,
            );
            return;
        };
        let role = match page.role {
            Some(SessionHistoryRole::User) => "user",
            Some(SessionHistoryRole::Assistant) => "assistant",
            None => "all",
        };
        frame.render_widget(
            Paragraph::new(vec![
                Line::styled(format!("Results for {:?} · {role} · seq {}", page.query, page.through_sequence), self.palette.label),
                Line::styled(format!("{} scanned · {} oversized skipped · U user / A assistant / B both / PgDn more", page.scanned_turns, page.skipped_oversized_turns), self.palette.quiet),
            ]).wrap(Wrap { trim: false }),
            header,
        );
        let items = page
            .items
            .iter()
            .map(|item| {
                ListItem::new(vec![
                    Line::raw(format!(
                        "{}  ·  {}",
                        status(item.task.status).0,
                        clean(&item.task.input_preview)
                    )),
                    Line::styled(
                        format!(
                            "Session {} · completion {}",
                            item.task.session_id, item.completion_sequence
                        ),
                        self.palette.quiet,
                    ),
                ])
            })
            .collect::<Vec<_>>();
        if items.is_empty() {
            frame.render_widget(
                Paragraph::new("No matching history.").style(self.palette.quiet),
                list_area,
            );
        } else {
            let selected = match self.layer {
                Layer::History(index) => index,
                _ => 0,
            };
            let mut state = ListState::default().with_selected(Some(selected.min(items.len() - 1)));
            frame.render_stateful_widget(
                List::new(items)
                    .highlight_symbol("› ")
                    .highlight_style(self.palette.choice),
                list_area,
                &mut state,
            );
        }
    }

    fn draw_draft_context(
        &self,
        frame: &mut Frame<'_>,
        area: Rect,
        preview: Option<&TaskContext>,
        scroll: u16,
    ) {
        let options = &self.draft_context;
        let isolated = options.mode == ContextMode::Isolated;
        let scope = if isolated {
            "Isolated prompt"
        } else {
            "Session history"
        };
        let mut settings = vec![Line::styled(
            format!("s  Scope    {scope}"),
            self.palette.label,
        )];
        if !isolated {
            let memory = if options.disable_memory {
                "Off".into()
            } else if let Some(picks) = &options.memory_selection {
                format!("Pinned ({})", picks.len())
            } else {
                "Automatic".into()
            };
            let history = match options.detail {
                ContextDetail::Full => "Full records",
                ContextDetail::Conversation => "Conversation only",
            };
            let pin = options.through_sequence.map_or_else(
                || "Live".into(),
                |sequence| format!("Pinned at #{sequence}"),
            );
            settings.extend([
                Line::raw(format!("m  Memory   {memory}  ·  v Choose")),
                Line::raw(format!("h  History  {history}")),
                Line::raw(format!("p  Preview  {pin}")),
            ]);
        }
        settings.push(Line::styled("r  Refresh", self.palette.quiet));
        let [controls, result] = Layout::vertical([
            Constraint::Length(settings.len() as u16 + 1),
            Constraint::Min(0),
        ])
        .areas(area);
        frame.render_widget(Paragraph::new(settings), controls);
        let text =
            format_draft_context(preview, self.busy, &self.message, usize::from(result.width));
        render_text(frame, result, &text, scroll);
    }

    pub(super) fn draw_layer(&self, frame: &mut Frame<'_>, area: Rect) {
        if matches!(
            self.layer,
            Layer::None
                | Layer::Inbox(_)
                | Layer::History(_)
                | Layer::TaskBrowser(_)
                | Layer::Edit(Edit::Task)
        ) {
            return;
        }
        let (max_width, max_height) = match &self.layer {
            Layer::Commands(menu) if menu.origin == CommandOrigin::Slash => {
                (58, (self.filtered_commands(menu).len() as u16 + 4).max(6))
            }
            Layer::Commands(_) => (66, 17),
            Layer::Language(_) => (54, 11),
            Layer::ModelPicker(picker) => (
                if picker.editing_connection() { 56 } else { 66 },
                picker.preferred_height(),
            ),
            Layer::Edit(Edit::Answer(..)) => (76, 15),
            Layer::Edit(_) | Layer::ConfirmCancel(_) => (64, 6),
            Layer::DraftContext { .. } => (76, 19),
            Layer::Recovery(..) | Layer::Reconcile(_) | Layer::Backup(_) => (88, 19),
            Layer::Approval { .. } => (88, 21),
            Layer::ReadRules(..) => (76, 17),
            Layer::ProjectChecks(_) | Layer::ProjectGuide(_) => (88, 21),
            Layer::ProjectReview { .. } => (100, u16::MAX),
            _ => (104, u16::MAX),
        };
        let top_gap = u16::from(area.height >= 22);
        let width = area.width.min(max_width);
        let height = area.height.saturating_sub(top_gap).min(max_height);
        let y = if matches!(&self.layer, Layer::ModelPicker(_) | Layer::Language(_))
            || matches!(&self.layer, Layer::Commands(menu) if menu.origin != CommandOrigin::Workbench)
        {
            area.bottom().saturating_sub(height)
        } else {
            area.y + top_gap
        };
        // At wide terminal sizes leave the left column of the transcript visible.
        let x = if area.width >= 100
            && matches!(&self.layer, Layer::ModelPicker(picker) if picker.editing_connection())
        {
            area.right().saturating_sub(width).saturating_sub(1)
        } else {
            area.x + (area.width - width) / 2
        };
        let modal = Rect::new(x, y, width, height);
        frame.render_widget(Clear, modal);
        let (title, actions) = match &self.layer {
            Layer::None | Layer::TaskBrowser(_) => return,
            Layer::Commands(menu) if menu.origin == CommandOrigin::Slash => (
                self.language.text("Commands", "命令").into(),
                self.language.text(
                    "Tab run  ·  Enter keep as text  ·  Esc back",
                    "Tab 执行 · Enter 保留为文本 · Esc 返回",
                ),
            ),
            Layer::Commands(menu) => (
                format!(
                    "{}  /  {}",
                    self.language.text("Commands", "命令"),
                    menu.query
                ),
                self.language.text(
                    "Type to filter  ·  Enter run  ·  Esc back",
                    "输入筛选 · Enter 执行 · Esc 返回",
                ),
            ),
            Layer::ModelPicker(picker) => (picker.title().into(), picker.actions()),
            Layer::Language(_) => (
                self.language.text("Interface language", "界面语言").into(),
                self.language.text(
                    "↑↓ Select · Enter Apply · Esc Back",
                    "↑↓ 选择 · Enter 应用 · Esc 返回",
                ),
            ),
            Layer::Inbox(_) => (
                "Workspace inbox".into(),
                "1 attention / 2 running / 3 recent / Enter open / h history",
            ),
            Layer::History(_) => (
                "Workspace history".into(),
                "Enter open / PgDn more / U user / A assistant / B both",
            ),
            Layer::Workspaces(..) => ("Workspaces".into(), "Open folder"),
            Layer::Sessions(..) => (
                format!(
                    "{}{}",
                    if self.session_archived {
                        "Archived sessions"
                    } else {
                        "Sessions"
                    },
                    self.session_query
                        .as_deref()
                        .map_or_else(String::new, |q| format!("  /  {}", clean(q)))
                ),
                "Enter open / f find / A archive view / r rename / x archive or restore / n new",
            ),
            Layer::Artifacts(_) => ("Artifacts".into(), ""),
            Layer::Timeline(page, _) => (
                page.as_ref().map_or_else(
                    || "Task timeline".into(),
                    |p| format!("Task timeline · through #{}", p.through_sequence),
                ),
                "Enter details / a approval / PgDn more / Home first / r refresh",
            ),
            Layer::TimelineEvent { sequence, .. } => (format!("Event #{sequence}"), "Esc timeline"),
            Layer::Context(..) => ("Context and memory usage".into(), "Esc back"),
            Layer::ProjectChecks(_) => (
                self.language.text("Project checks", "项目检查").into(),
                self.language.text(
                    "↑↓ Scroll · R Refresh · Esc Back",
                    "↑↓ 滚动 · R 刷新 · Esc 返回",
                ),
            ),
            Layer::ProjectGuide(panel) => (
                self.language
                    .text("Project guide · AGENTS.md", "项目指南 · AGENTS.md")
                    .into(),
                panel.actions(self.language),
            ),
            Layer::DraftContext { .. } => ("Draft context".into(), "Esc Return to draft"),
            Layer::Queue(queue, _) => (
                queue.as_ref().map_or_else(
                    || "Queued prompts".into(),
                    |q| {
                        let state = if q.archived {
                            " · Archived"
                        } else if q.blocking_task_id.is_some() {
                            " · Task in progress"
                        } else {
                            ""
                        };
                        format!("Queued prompts ({}){state}", q.pending.len())
                    },
                ),
                "Enter start first / Del remove / g blocking task / r refresh",
            ),
            Layer::Approval {
                review, remember, ..
            } => (
                self.language.text("Review operation", "审阅操作").into(),
                if self.busy {
                    self.language
                        .text("Loading review…  ·  Esc Back", "正在加载审阅… · Esc 返回")
                } else if review.allow_once && remember.is_some() {
                    self.language.text(
                        "y Allow once  ·  n Deny  ·  Esc Back\nr Remember read for 1 hour",
                        "y 允许一次 · n 拒绝 · Esc 返回\nr 记住读取授权一小时",
                    )
                } else if review.allow_once {
                    self.language.text(
                        "y Allow once  ·  n Deny  ·  Esc Back",
                        "y 允许一次 · n 拒绝 · Esc 返回",
                    )
                } else {
                    self.language
                        .text("n Deny  ·  Esc Back", "n 拒绝 · Esc 返回")
                },
            ),
            Layer::ProjectReview { .. } => (
                self.language
                    .text("Project changes & checks", "项目改动与检查")
                    .into(),
                self.language.text(
                    "↑↓ Scroll  ·  Home Top  ·  Esc Back",
                    "↑↓ 滚动 · Home 顶部 · Esc 返回",
                ),
            ),
            Layer::ReadRules(..) => (
                self.language.text("Read permissions", "读取权限").into(),
                self.language.text(
                    "↑↓ Select  ·  Del Revoke  ·  Esc Back",
                    "↑↓ 选择 · Del 撤销 · Esc 返回",
                ),
            ),
            Layer::Recovery(id, _) => {
                let actions = match self.detail.as_ref().filter(|detail| detail.task.id == *id) {
                    Some(detail) if detail.can_reconcile && detail.review.is_none() => {
                        "o Record outcome  ·  Esc Back"
                    }
                    Some(detail) => match detail.resume.as_ref().map(|resume| resume.disposition) {
                        Some(TaskResumeDisposition::Ready) => "y Resume  ·  Esc Back",
                        Some(TaskResumeDisposition::RetryUncertainModelStep) => {
                            "y Confirm retry  ·  Esc Back"
                        }
                        _ => "Esc Back",
                    },
                    None => "Esc Back",
                };
                ("Task recovery".into(), actions)
            }
            Layer::Memories(panel) => ("Workspace memories".into(), panel.help()),
            Layer::Deliverables(catalog) => (catalog.title().into(), catalog.help()),
            Layer::Diagnostics(..) => (
                "Connection and configuration".into(),
                "↑/↓ Scroll  ·  Esc Back",
            ),
            Layer::Backup(_) => (
                "Database backups".into(),
                "c Create  ·  v Verify  ·  r Restore  ·  Esc Back",
            ),
            Layer::Reconcile(_) => (
                "Record external outcome".into(),
                "Tab Next field  ·  Ctrl+S Record  ·  Esc Back",
            ),
            Layer::ConfirmCancel(_) => {
                ("Cancel task?".into(), "y Cancel task  ·  Esc Keep running")
            }
            Layer::Preview(preview, _) => (
                "Artifact preview".into(),
                match (preview.text.is_some(), preview.revision.is_some()) {
                    (true, true) => "Save / Use as input / Previous",
                    (true, false) => "Save / Use as input",
                    (false, true) => "Save / Previous",
                    (false, false) => "Save",
                },
            ),
            Layer::Edit(edit) => (edit.name().into(), "Enter Confirm  ·  Esc Back"),
        };
        let title = if self.busy {
            format!(" {title} · {} ", self.language.text("Working", "处理中"))
        } else {
            format!(" {title} ")
        };
        let block = Block::default()
            .style(self.palette.rail)
            .border_style(self.palette.quiet)
            .borders(Borders::ALL)
            .border_type(ratatui::widgets::BorderType::Rounded)
            .title(Span::styled(title, self.palette.label));
        let inset = u16::from(area.width >= 55 || !matches!(self.layer, Layer::Approval { .. }));
        let body = block.inner(modal).inner(Margin::new(inset, 0));
        frame.render_widget(block, modal);
        if body.is_empty() {
            return;
        }
        let body = if actions.is_empty() {
            body
        } else {
            let actions = Paragraph::new(actions)
                .wrap(Wrap { trim: true })
                .style(self.palette.label);
            let footer_height = actions
                .line_count(body.width)
                .min(usize::from(body.height.saturating_sub(1)))
                .min(2) as u16;
            let [body, footer] =
                Layout::vertical([Constraint::Min(0), Constraint::Length(footer_height)])
                    .areas(body);
            frame.render_widget(actions, footer);
            body
        };
        match &self.layer {
            Layer::None => {}
            Layer::Inbox(_) | Layer::History(_) | Layer::TaskBrowser(_) => {}
            Layer::DraftContext { preview, scroll } => {
                self.draw_draft_context(frame, body, preview.as_deref(), *scroll)
            }
            Layer::Commands(menu) => {
                let commands = self.filtered_commands(menu);
                if commands.is_empty() {
                    render_text(
                        frame,
                        body,
                        self.language.text("No matching commands", "没有匹配的命令"),
                        0,
                    );
                } else {
                    render_list(
                        frame,
                        body,
                        commands
                            .into_iter()
                            .map(|command| command.caption(body.width.saturating_sub(2)))
                            .collect(),
                        menu.index,
                        self.palette.choice,
                    );
                }
            }
            Layer::Workspaces(page, index) => render_list(
                frame,
                body,
                page.as_ref()
                    .map(|p| p.items.iter().map(|item| clean(&item.root)).collect())
                    .unwrap_or_default(),
                *index,
                self.palette.choice,
            ),
            Layer::Sessions(page, index) => render_list(
                frame,
                body,
                page.as_ref()
                    .map(|p| p.items.iter().map(session_name).collect())
                    .unwrap_or_default(),
                *index,
                self.palette.choice,
            ),
            Layer::Timeline(page, index) => render_list(
                frame,
                body,
                page.as_ref().map_or_else(Vec::new, |page| {
                    page.entries
                        .iter()
                        .map(|entry| {
                            let label = clean(&entry.kind.replace('_', " "));
                            let tool = entry
                                .tool_name
                                .as_ref()
                                .map_or_else(String::new, |name| format!("  ·  {}", clean(name)));
                            let approval = entry
                                .approval_id
                                .as_ref()
                                .map_or_else(String::new, |id| format!("  ·  {}", clean(id)));
                            format!("#{}  {label}{tool}{approval}", entry.sequence)
                        })
                        .collect()
                }),
                *index,
                self.palette.choice,
            ),
            Layer::TimelineEvent { text, scroll, .. } | Layer::Context(text, scroll) => {
                render_text(frame, body, text, *scroll)
            }
            Layer::Queue(queue, index) => render_list(
                frame,
                body,
                queue.as_ref().map_or_else(Vec::new, |queue| {
                    queue
                        .pending
                        .iter()
                        .map(|item| {
                            let mode = if item.plan_only { "Plan" } else { "Execution" };
                            let text = clean(&item.input_preview).replace('\n', " ");
                            format!("{mode}  ·  {text}")
                        })
                        .collect()
                }),
                *index,
                self.palette.choice,
            ),
            Layer::Artifacts(index) => render_list(
                frame,
                body,
                self.detail
                    .as_ref()
                    .map(|d| {
                        d.artifacts
                            .iter()
                            .map(|a| {
                                format!(
                                    "{}  |  {}  |  {} B",
                                    clean(&a.title),
                                    clean(&a.media_type),
                                    a.content_bytes
                                )
                            })
                            .collect()
                    })
                    .unwrap_or_default(),
                *index,
                self.palette.choice,
            ),
            Layer::Approval { review, .. } => {
                review.draw(frame, body, self.language, &self.palette)
            }
            Layer::ProjectReview { scroll, .. } => self.draw_project_review(frame, body, *scroll),
            Layer::ReadRules(rules, index) => match rules {
                Some(rules) if !rules.is_empty() => {
                    let active = self.session.as_ref().map(|session| session.id.as_str());
                    let items = rules
                        .iter()
                        .map(|rule| {
                            let scope = match rule.session_id.as_deref() {
                                None => "workspace",
                                Some(session) if Some(session) == active => "this conversation",
                                Some(_) => "another conversation",
                            };
                            let target = rule.arguments["path"].as_str().unwrap_or(&rule.tool_name);
                            let target = clean(target).replace('\n', " ");
                            format!(
                                "{}\n    {}",
                                ellipsis(&target, usize::from(body.width.saturating_sub(2))),
                                scope
                            )
                        })
                        .collect();
                    render_list(frame, body, items, *index, self.palette.choice);
                }
                Some(_) => render_text(frame, body, "No active read permissions", 0),
                None => render_text(
                    frame,
                    body,
                    if self.message.is_empty() {
                        "Loading read permissions…"
                    } else {
                        &self.message
                    },
                    0,
                ),
            },
            Layer::Recovery(id, scroll) => {
                if let Some(detail) = self.detail.as_ref().filter(|detail| detail.task.id == *id) {
                    let mut text = detail.resume.as_ref().map_or_else(String::new, |status| {
                        disposition_text(status.disposition).into()
                    });
                    if let Some(run) = &detail.task.agent_run {
                        text.push_str(&format!("\n\nAgent run {}", clean(&run.id)));
                        if let Some(remote) = &run.remote_task {
                            text.push_str(&format!(
                                "\nRemote task {} · {:?}",
                                clean(&remote.id),
                                remote.state
                            ));
                        }
                    }
                    if let Some(review) = &detail.review {
                        text.push_str(&format!(
                            "\n\nRecorded outcome: {}\nOperator: {}\nEvidence: {}",
                            outcome_label(review.review.state),
                            clean(&review.review.operator),
                            clean(&review.review.evidence)
                        ));
                    } else if detail.can_reconcile {
                        text.push_str("\n\nVerify the result with the external Agent before recording an outcome. The review is added to the task's audit history.");
                    }
                    render_text(frame, body, text.trim(), *scroll);
                }
            }
            Layer::Memories(panel) => self.draw_memories(frame, body, panel),
            Layer::Deliverables(catalog) => self.draw_deliverables(frame, body, catalog),
            Layer::ModelPicker(picker) => picker.draw(frame, body, &self.palette),
            Layer::Language(panel) => panel.draw(frame, body, self.language, &self.palette),
            Layer::ProjectChecks(panel) => {
                panel.draw(frame, body, self.language, &self.palette, &self.message)
            }
            Layer::ProjectGuide(panel) => {
                panel.draw(frame, body, self.language, &self.palette, &self.message)
            }
            Layer::Diagnostics(text, scroll) => {
                let state = if self.online {
                    ""
                } else {
                    "Service: Disconnected\n\n"
                };
                let text = if self.busy && text.is_empty() {
                    "Checking local configuration…".into()
                } else {
                    format!("{state}{}", clean(text))
                };
                render_text(frame, body, &text, *scroll);
            }
            Layer::Backup(result) => {
                let text = if let Some(result) = result {
                    let manifest = &result.manifest;
                    let reopen = if matches!(result.request, BackupRequest::Restore { .. }) {
                        "\nOpen this copy with fliuno --db PATH."
                    } else {
                        ""
                    };
                    format!(
                        "{}\n{}{reopen}\n\nSchema {}  ·  Through event #{}\nTasks: {}  ·  Deliverables: {}  ·  {} bytes\n\nSHA-256\n{}",
                        result.request.label(),
                        clean(&result.request.result_path().to_string_lossy()),
                        manifest.schema_version,
                        manifest.event_through_sequence,
                        manifest.task_count,
                        manifest.artifact_count,
                        manifest.database_bytes,
                        clean(&manifest.database_sha256)
                    )
                } else if self.busy {
                    "Working on the database backup…".into()
                } else if !self.message.is_empty() {
                    clean(&self.message)
                } else {
                    "Create a verified snapshot of work saved in this database.\n\nVerify an existing snapshot or restore it to a new database path.\n\nSaved conversations, approvals and deliverables are included.".into()
                };
                render_text(frame, body, &text, 0);
            }
            Layer::Reconcile(form) => self.draw_reconciliation(frame, body, form),
            Layer::ConfirmCancel(_) => {}
            Layer::Preview(preview, scroll) => {
                let revision = preview
                    .revision
                    .as_ref()
                    .map(|r| format!("\nPrevious revision: {}", r.previous.artifact_id))
                    .unwrap_or_default();
                let text = format!(
                    "{} bytes\nSHA-256: {}{revision}\n\n{}",
                    preview.digest.content_bytes,
                    preview.digest.sha256,
                    preview.text.as_deref().unwrap_or("No text preview")
                );
                render_text(frame, body, &text, *scroll);
            }
            Layer::Edit(edit) => {
                let input = if let Edit::Answer(_, _, question) = edit {
                    let [prompt, input] =
                        Layout::vertical([Constraint::Length(body.height / 2), Constraint::Min(0)])
                            .areas(body);
                    frame.render_widget(
                        Paragraph::new(clean(question))
                            .wrap(Wrap { trim: false })
                            .style(self.palette.quiet),
                        prompt,
                    );
                    input
                } else {
                    body
                };
                if !input.is_empty() {
                    let (lines, x, y) = self.input.viewport(input.width, input.height);
                    frame.render_widget(
                        Paragraph::new(lines.into_iter().map(Line::raw).collect::<Vec<_>>()),
                        input,
                    );
                    frame.set_cursor_position(Position::new(input.x + x, input.y + y));
                }
            }
        }
    }

    fn draw_reconciliation(&self, frame: &mut Frame<'_>, body: Rect, form: &ReconcileForm) {
        use crate::recovery::ReconcileField;
        let [
            outcome,
            operator_label,
            operator,
            evidence_label,
            evidence,
            notice,
        ] = Layout::vertical([
            Constraint::Length(2),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(0),
            Constraint::Length(u16::from(!self.message.is_empty()) * 2),
        ])
        .areas(body);
        render_text(frame, notice, &clean(&self.message), 0);
        let style = |field| {
            if form.field == field {
                self.palette.label
            } else {
                self.palette.quiet
            }
        };
        let headline = form
            .state
            .map_or("Choose a terminal outcome", outcome_label);
        let selection = format!("Outcome: {headline}  ·  ←/→ Change");
        frame.render_widget(
            Paragraph::new(selection)
                .style(style(ReconcileField::Outcome))
                .wrap(Wrap { trim: false }),
            outcome,
        );
        frame.render_widget(
            Paragraph::new("Operator").style(style(ReconcileField::Operator)),
            operator_label,
        );
        frame.render_widget(
            Paragraph::new("Evidence").style(style(ReconcileField::Evidence)),
            evidence_label,
        );
        for (area, editor, field) in [
            (operator, &form.operator, ReconcileField::Operator),
            (evidence, &form.evidence, ReconcileField::Evidence),
        ] {
            if !area.is_empty() {
                let (lines, x, y) = editor.viewport(area.width, area.height);
                frame.render_widget(
                    Paragraph::new(lines.into_iter().map(Line::raw).collect::<Vec<_>>()),
                    area,
                );
                if form.field == field {
                    frame.set_cursor_position(Position::new(area.x + x, area.y + y));
                }
            }
        }
    }
}
