use super::*;
pub(crate) use fliuno_config::Language;
use fliuno_config::Settings;

pub(super) trait LanguageText {
    fn text(self, en: &'static str, zh: &'static str) -> &'static str;
}

impl LanguageText for Language {
    fn text(self, en: &'static str, zh: &'static str) -> &'static str {
        match self {
            Self::English => en,
            Self::SimplifiedChinese => zh,
        }
    }
}

const LANGUAGES: [(Language, &str); 2] = [
    (Language::English, "English"),
    (Language::SimplifiedChinese, "简体中文"),
];

pub(super) struct Panel {
    origin: CommandOrigin,
    selected: usize,
    result: Result<Option<bool>, String>,
}

impl Panel {
    pub fn new(origin: CommandOrigin, language: Language) -> Self {
        Self {
            origin,
            selected: LANGUAGES
                .iter()
                .position(|(item, _)| *item == language)
                .unwrap_or(0),
            result: Ok(None),
        }
    }

    pub fn draw(&self, frame: &mut Frame<'_>, body: Rect, language: Language, palette: &Palette) {
        let [choices, note] =
            Layout::vertical([Constraint::Length(3), Constraint::Min(0)]).areas(body);
        render_list(
            frame,
            choices,
            LANGUAGES
                .iter()
                .map(|(item, name)| {
                    if *item == language {
                        format!("{name}  ·  {}", language.text("current", "当前"))
                    } else {
                        (*name).to_owned()
                    }
                })
                .collect(),
            self.selected,
            palette.choice,
        );
        let (message, style) = match &self.result {
            Ok(None) => (
                language
                    .text(
                        "Changes the interface for you. Model replies stay as written.",
                        "仅切换界面显示，模型回复保留原文。",
                    )
                    .to_owned(),
                palette.quiet,
            ),
            Ok(Some(true)) => (
                language
                    .text("Saved for future launches.", "已保存，下次启动仍然生效。")
                    .to_owned(),
                palette.status(Color::Green),
            ),
            Ok(Some(false)) => (
                language
                    .text(
                        "Applied for this demo session.",
                        "已应用，仅在本次演示有效。",
                    )
                    .to_owned(),
                palette.label,
            ),
            Err(error) => (
                format!(
                    "{}: {}",
                    language.text("Could not save language", "无法保存界面语言"),
                    clean(error)
                ),
                palette.status(Color::Red),
            ),
        };
        frame.render_widget(
            Paragraph::new(Line::styled(message, style)).wrap(Wrap { trim: true }),
            note,
        );
    }
}

impl App {
    pub(super) fn language_key(&mut self, event: KeyEvent) {
        let Layer::Language(panel) = &mut self.layer else {
            return;
        };
        step(event.code, &mut panel.selected, LANGUAGES.len());
        if event.kind != KeyEventKind::Press {
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
            KeyCode::Enter => {
                let (language, _) = LANGUAGES[panel.selected];
                panel.result = if let Some(path) = &self.config_path {
                    Settings::save_workbench_language(path, language).map(|()| Some(true))
                } else {
                    Ok(Some(false))
                };
                if panel.result.is_ok() {
                    self.language = language;
                }
            }
            _ => (),
        }
    }
}
