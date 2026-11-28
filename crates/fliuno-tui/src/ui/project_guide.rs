use super::*;
use fliuno_protocol::{WorkspaceProjectGuidePreview, WorkspaceSetProjectGuideParams};

pub(super) struct Panel {
    origin: CommandOrigin,
    preview: Option<WorkspaceProjectGuidePreview>,
    review: Option<approval::Review>,
}

impl Panel {
    pub(super) fn actions(&self, language: Language) -> &'static str {
        match &self.preview {
            Some(preview) if preview.guide.is_some() => {
                if preview.state.enabled_sha256.is_some() {
                    language.text(
                        "Enter Use · D Disable · R Reload · Esc",
                        "Enter 使用本版 · D 停用 · R 刷新 · Esc 返回",
                    )
                } else {
                    language.text(
                        "Enter Enable · R Reload · Esc Back",
                        "Enter 启用 · R 刷新 · Esc 返回",
                    )
                }
            }
            Some(preview) if preview.state.enabled_sha256.is_some() => language.text(
                "D Disable · R Reload · Esc Back",
                "D 停用 · R 刷新 · Esc 返回",
            ),
            _ => language.text("R Reload · Esc Back", "R 刷新 · Esc 返回"),
        }
    }

    pub(super) fn draw(
        &self,
        frame: &mut Frame<'_>,
        body: Rect,
        language: Language,
        palette: &Palette,
        error: &str,
    ) {
        let mut body = body;
        if !error.is_empty() {
            let [warning, rest] =
                Layout::vertical([Constraint::Length(2), Constraint::Min(0)]).areas(body);
            frame.render_widget(
                Paragraph::new(clean(error))
                    .wrap(Wrap { trim: true })
                    .style(palette.status(Color::Red)),
                warning,
            );
            body = rest;
        }
        if let Some(review) = &self.review {
            review.draw(frame, body, language, palette);
        } else if error.is_empty() {
            render_text(
                frame,
                body,
                language.text("Reading the project guide…", "正在读取项目指南…"),
                0,
            );
        }
    }

    fn install(&mut self, preview: WorkspaceProjectGuidePreview, language: Language) {
        let current = preview.state.enabled_sha256.as_deref();
        let status = match (current, preview.guide.as_ref()) {
            (None, _) => language.text("Disabled", "未启用"),
            (Some(hash), Some(guide)) if guide.sha256 == hash => {
                language.text("Enabled for this workspace", "已为此工作区启用")
            }
            _ => language.text(
                "Changed or unavailable · review again",
                "内容已变化或不可用 · 请重新审阅",
            ),
        };
        let mut summary = format!(
            "{status}\n{}\n\n",
            language.text(
                "Enabled text is sent with new model requests. It grants no tool permissions.",
                "启用的正文会随新的模型请求发送，不会授予工具权限。"
            )
        );
        // Keep lines of the source intact while wrapping them to terminal width.
        let offset = summary.len();
        let details = match &preview.guide {
            Some(guide) => {
                summary.push_str(&approval::display_text(&guide.text));
                format!(
                    "{}: {}\nSHA-256: {}\n{}: {}",
                    language.text("Source", "来源"),
                    approval::display_text(&guide.path),
                    approval::display_text(&guide.sha256),
                    language.text("Bytes", "字节数"),
                    guide.text.len()
                )
            }
            None => {
                summary.push_str(preview.read_error.as_deref().unwrap_or_else(|| {
                    language.text(
                        "No non-empty AGENTS.md in the workspace root.",
                        "工作区根目录没有非空的 AGENTS.md。",
                    )
                }));
                String::new()
            }
        };
        self.review = Some(approval::Review::new(summary, details, false).verbatim_from(offset));
        self.preview = Some(preview);
    }
}

impl App {
    pub(super) fn enabled_project_guide(&self) -> Option<u64> {
        let state = self.project_guide.as_ref()?;
        (self.workspace.as_ref()?.id == state.workspace_id && state.enabled_sha256.is_some())
            .then_some(state.revision)
    }

    pub(super) fn project_context_options(&self) -> ContextOptions {
        let mut options = self.draft_context.clone();
        options.project_guide_revision = if options.mode == ContextMode::Session {
            self.enabled_project_guide()
        } else {
            None
        };
        options
    }

    pub(super) fn open_project_guide(&mut self, origin: CommandOrigin, worker: &Worker) -> bool {
        let Some(workspace) = self.workspace.as_ref().map(|w| w.id.clone()) else {
            return false;
        };
        if !self.send(worker, Request::ProjectGuide(workspace)) {
            return false;
        }
        self.message.clear();
        self.layer = Layer::ProjectGuide(Box::new(Panel {
            origin,
            preview: None,
            review: None,
        }));
        true
    }

    pub(super) fn install_project_guide(&mut self, preview: WorkspaceProjectGuidePreview) {
        if self
            .workspace
            .as_ref()
            .is_none_or(|workspace| workspace.id != preview.state.workspace_id)
        {
            return;
        }
        if self.project_guide.as_ref() != Some(&preview.state) {
            self.key = uuid::Uuid::new_v4().to_string();
        }
        self.project_guide = Some(preview.state.clone());
        if let Layer::ProjectGuide(panel) = &mut self.layer {
            panel.install(preview, self.language);
        }
    }

    pub(super) fn project_guide_key(&mut self, event: KeyEvent, worker: &Worker) {
        let pressed = event.kind == KeyEventKind::Press;
        let Layer::ProjectGuide(panel) = &mut self.layer else {
            return;
        };
        if let Some(review) = &mut panel.review {
            review.navigate(event.code, pressed);
        }
        if !pressed || !event.modifiers.is_empty() {
            return;
        }
        if event.code == KeyCode::Esc {
            self.layer = if panel.origin == CommandOrigin::Workbench {
                Layer::None
            } else {
                Layer::Edit(Edit::Task)
            };
            return;
        }
        let request = match (event.code, &panel.preview) {
            (KeyCode::Char('r'), _) => self
                .workspace
                .as_ref()
                .map(|workspace| Request::ProjectGuide(workspace.id.clone())),
            (KeyCode::Enter, Some(preview)) => preview.guide.as_ref().map(|guide| {
                Request::SaveProjectGuide(WorkspaceSetProjectGuideParams {
                    workspace_id: preview.state.workspace_id.clone(),
                    expected_revision: preview.state.revision,
                    sha256: Some(guide.sha256.clone()),
                })
            }),
            (KeyCode::Char('d'), Some(preview)) if preview.state.enabled_sha256.is_some() => {
                Some(Request::SaveProjectGuide(WorkspaceSetProjectGuideParams {
                    workspace_id: preview.state.workspace_id.clone(),
                    expected_revision: preview.state.revision,
                    sha256: None,
                }))
            }
            _ => None,
        };
        if let Some(request) = request
            && self.send(worker, request)
        {
            self.message.clear();
        }
    }
}
