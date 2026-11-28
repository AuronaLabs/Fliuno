use super::*;
use fliuno_protocol::{ProjectCheckInputStatus, WorkspaceProjectChecks};

pub(super) struct Panel {
    origin: CommandOrigin,
    review: Option<approval::Review>,
}

impl Panel {
    pub(super) fn draw(
        &self,
        frame: &mut Frame<'_>,
        body: Rect,
        language: Language,
        palette: &Palette,
        error: &str,
    ) {
        if !error.is_empty() {
            frame.render_widget(
                Paragraph::new(clean(error))
                    .wrap(Wrap { trim: true })
                    .style(palette.status(Color::Red)),
                body,
            );
        } else if let Some(review) = &self.review {
            review.draw(frame, body, language, palette);
        } else {
            render_text(
                frame,
                body,
                language.text("Reading project checks…", "正在读取项目检查…"),
                0,
            );
        }
    }
}

impl App {
    pub(super) fn open_project_checks(&mut self, origin: CommandOrigin, worker: &Worker) -> bool {
        let Some(workspace) = self.workspace.as_ref().map(|w| w.id.clone()) else {
            return false;
        };
        if !self.send(worker, Request::ProjectChecks(workspace)) {
            return false;
        }
        self.message.clear();
        self.layer = Layer::ProjectChecks(Box::new(Panel {
            origin,
            review: None,
        }));
        true
    }

    pub(super) fn install_project_checks(&mut self, checks: WorkspaceProjectChecks) {
        if self
            .workspace
            .as_ref()
            .is_none_or(|w| w.id != checks.workspace_id)
        {
            return;
        }
        if let Layer::ProjectChecks(panel) = &mut self.layer {
            let (summary, details) = catalog_text(checks, self.language);
            panel.review = Some(approval::Review::new(summary, details, false));
        }
    }

    pub(super) fn project_checks_key(&mut self, event: KeyEvent, worker: &Worker) {
        let Layer::ProjectChecks(panel) = &mut self.layer else {
            return;
        };
        let pressed = event.kind == KeyEventKind::Press;
        if let Some(review) = &mut panel.review {
            review.navigate(event.code, pressed);
        }
        if !pressed || !event.modifiers.is_empty() {
            return;
        }
        match event.code {
            KeyCode::Esc => {
                self.layer = if panel.origin == CommandOrigin::Workbench {
                    Layer::None
                } else {
                    Layer::Edit(Edit::Task)
                };
            }
            KeyCode::Char('r') => {
                if let Some(workspace) = self.workspace.as_ref().map(|w| w.id.clone())
                    && self.send(worker, Request::ProjectChecks(workspace))
                {
                    self.message.clear();
                }
            }
            _ => {}
        }
    }
}

fn catalog_text(catalog: WorkspaceProjectChecks, language: Language) -> (String, String) {
    use std::fmt::Write as _;
    if catalog.checks.is_empty() {
        return (language.text(
            "No project checks registered.\n\nSet [integrations].execution in config.toml, then restart Fliuno. See Project checks in the setup guide.",
            "尚未注册项目检查。\n\n在 config.toml 中设置 [integrations].execution，然后重启 Fliuno。参见部署指南中的项目检查。",
        ).into(), String::new());
    }
    let mut text = language
        .text(
            "Ask the agent to run a listed check. Review the request before it runs.\n",
            "可让 Agent 运行下列检查。开始前仍需审阅执行请求。\n",
        )
        .to_owned();
    let single_line = |s: &str| approval::display_text(s).replace('\n', " ");
    let mut details = String::new();
    for check in catalog.checks {
        let _ = writeln!(
            text,
            "\n{}\n  {}",
            single_line(&check.label),
            single_line(&check.description)
        );
        let _ = writeln!(
            details,
            "{}\n  {}\n",
            single_line(&check.label),
            single_line(&check.tool_name)
        );
        for input in check.inputs {
            let status = match input.status {
                ProjectCheckInputStatus::Present => language.text("Present", "存在"),
                ProjectCheckInputStatus::Missing => language.text("Missing", "缺失"),
                ProjectCheckInputStatus::Unavailable => {
                    language.text("Unavailable", "无法读取状态")
                }
                ProjectCheckInputStatus::Unsupported => {
                    language.text("Unsupported", "不支持的类型")
                }
            };
            let _ = writeln!(text, "  {status} · {}", single_line(&input.path));
        }
    }
    if catalog.truncated {
        text.push_str(language.text(
            "\nMore checks exist beyond this list.",
            "\n还有更多检查未列出。",
        ));
    }
    (text, details)
}
