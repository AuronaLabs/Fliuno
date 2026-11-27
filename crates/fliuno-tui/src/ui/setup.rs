use super::{clean, palette::Palette};
use crate::{Screen, editor::Editor, options::SetupOptions};
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use fliuno_config::{PendingApiKey, Provider, read_api_key};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Margin, Position, Rect},
    style::Color,
    text::Line,
    widgets::{Block, Paragraph, Wrap},
};
use std::{
    fs,
    io::{self, Write},
    path::Path,
};
use zeroize::{Zeroize, Zeroizing};

const PROVIDERS: [(Provider, &str); 3] = [
    (Provider::OpenAi, "OpenAI"),
    (Provider::Anthropic, "Anthropic"),
    (Provider::Gemini, "Google Gemini"),
];

#[derive(Default)]
enum Step {
    #[default]
    Provider,
    Model,
    Credential,
}

#[derive(Default)]
struct Form {
    step: Step,
    provider: usize,
    model: Editor,
    key_file: Editor,
    use_key_file: bool,
    api_key: Zeroizing<String>,
    error: String,
}

pub fn run(screen: &mut Screen, options: SetupOptions) -> io::Result<bool> {
    let mut form = Form::default();
    let palette = Palette::new(options.theme);
    loop {
        screen.draw(|frame| form.draw(frame, &options.config, &palette))?;
        match event::read()? {
            Event::Paste(mut text) => {
                form.insert(&text);
                text.zeroize();
            }
            Event::Key(key) if key.kind != KeyEventKind::Release => {
                let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
                match key.code {
                    KeyCode::Char('c') if ctrl => return Ok(false),
                    KeyCode::Esc => {
                        form.error.clear();
                        form.api_key.zeroize();
                        form.step = match form.step {
                            Step::Provider => return Ok(false),
                            Step::Model => Step::Provider,
                            Step::Credential => Step::Model,
                        };
                    }
                    KeyCode::F(2) if matches!(form.step, Step::Credential) => {
                        form.use_key_file = !form.use_key_file;
                        form.api_key.zeroize();
                        form.error.clear();
                    }
                    KeyCode::Enter if key.kind == KeyEventKind::Press => {
                        form.error.clear();
                        match form.step {
                            Step::Provider => form.step = Step::Model,
                            Step::Model if form.model.text().trim().is_empty() => {
                                form.error = "Enter a model ID available on your account.".into();
                            }
                            Step::Model => form.step = Step::Credential,
                            Step::Credential => match form.save(&options.config) {
                                Ok(()) => return Ok(true),
                                Err(error) => form.error = error,
                            },
                        }
                    }
                    KeyCode::Char(ch) if !ctrl && !key.modifiers.contains(KeyModifiers::ALT) => {
                        form.insert(ch.encode_utf8(&mut [0; 4]));
                    }
                    _ => form.navigate(key.code),
                }
            }
            _ => {}
        }
    }
}

impl Form {
    fn field(&mut self) -> Option<&mut Editor> {
        match self.step {
            Step::Model => Some(&mut self.model),
            Step::Credential if self.use_key_file => Some(&mut self.key_file),
            _ => None,
        }
    }

    fn insert(&mut self, text: &str) {
        let text = text.trim_matches(['\r', '\n']);
        if matches!(self.step, Step::Credential) && !self.use_key_file {
            if self.api_key.len() + text.len() > 8192
                || text.chars().any(|ch| ch.is_whitespace() || ch.is_control())
            {
                self.error = "Paste a single API key without whitespace.".into();
            } else {
                self.api_key.push_str(text);
                self.error.clear();
            }
            return;
        }
        let limit = if matches!(self.step, Step::Model) {
            256
        } else {
            4096
        };
        if !text.chars().any(char::is_control)
            && let Some(field) = self.field()
            && field.text().len() + text.len() <= limit
            && field.insert(text, limit)
        {
            self.error.clear();
        }
    }

    fn navigate(&mut self, key: KeyCode) {
        if let Some(field) = self.field() {
            if field.key(key) {
                self.error.clear();
            }
        } else if matches!(self.step, Step::Credential) {
            if key == KeyCode::Backspace {
                self.api_key.pop();
                self.error.clear();
            }
        } else {
            match key {
                KeyCode::Up | KeyCode::Left => self.provider = self.provider.saturating_sub(1),
                KeyCode::Down | KeyCode::Right => {
                    self.provider = (self.provider + 1).min(PROVIDERS.len() - 1);
                }
                _ => {}
            }
        }
    }

    fn save(&self, config: &Path) -> Result<(), String> {
        let parent = config
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        let provider = PROVIDERS[self.provider].0.name();
        // JSON and TOML use the same string escapes for these single-line, control-free inputs.
        let model = serde_json::to_string(self.model.text().trim()).map_err(|e| e.to_string())?;
        let mut pending = None;
        let credential = if self.use_key_file {
            let key_file = self.key_file.text().trim();
            if key_file.is_empty() {
                return Err("Enter the path to your API key file.".into());
            }
            read_api_key(&parent.join(key_file))?;
            let file = serde_json::to_string(key_file).map_err(|e| e.to_string())?;
            format!("api_key_file = {file}")
        } else {
            if self.api_key.is_empty() {
                return Err("Paste your API key or press F2 to use a key file.".into());
            }
            let key = PendingApiKey::stage(config, &self.api_key)?;
            let setting = key.setting().to_owned();
            pending = Some(key);
            setting
        };
        let content = format!("[model]\nprovider = \"{provider}\"\nname = {model}\n{credential}\n");
        let write = || -> io::Result<()> {
            fs::create_dir_all(parent)?;
            let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
            temporary.write_all(content.as_bytes())?;
            temporary.as_file().sync_all()?;
            temporary.persist_noclobber(config)?;
            Ok(())
        };
        write().map_err(|error| {
            if error.kind() == io::ErrorKind::AlreadyExists {
                "A profile already exists. Restart Fliuno to open it.".into()
            } else {
                format!("Could not save profile: {error}")
            }
        })?;
        if let Some(key) = pending {
            key.commit();
        }
        Ok(())
    }

    fn draw(&self, frame: &mut Frame<'_>, config: &Path, palette: &Palette) {
        let viewport = frame.area();
        frame.render_widget(Block::default().style(palette.base), viewport);
        let width = viewport.width.min(68);
        let area = Rect::new(
            viewport.x + (viewport.width - width) / 2,
            viewport.y,
            width,
            viewport.height,
        )
        .inner(Margin::new(
            u16::from(width > 16) * 2,
            u16::from(viewport.height > 12),
        ));
        let (step, heading, description, help) = match self.step {
            Step::Provider => (
                1,
                "Choose your model provider",
                "Use a model from your own account.",
                "↑ ↓ Select   Enter Continue   Esc Exit",
            ),
            Step::Model => (
                2,
                "Enter a model ID",
                "Use a model ID from your provider account.",
                "Enter Continue   Esc Back",
            ),
            Step::Credential if self.use_key_file => (
                3,
                "Connect an API key file",
                "Enter an absolute path or one relative to the profile. Use a file with one API key.",
                "Enter Save   F2 Paste key   Esc Back",
            ),
            Step::Credential => (
                3,
                "Connect your API key",
                if cfg!(windows) {
                    "Paste a key from your model provider. Windows Credential Manager stores it for your account."
                } else {
                    "Paste a key from your model provider. Fliuno stores it in a private file next to your profile."
                },
                "Enter Save   F2 Key file   Esc Back",
            ),
        };
        let header = Paragraph::new(vec![
            Line::styled(format!("Fliuno  /  Set up  ·  {step} of 3"), palette.label),
            Line::raw(""),
            Line::styled(heading, palette.label),
            Line::styled(description, palette.quiet),
        ])
        .wrap(Wrap { trim: false });
        let [header_area, body, footer_area, _] = Layout::vertical([
            Constraint::Length(header.line_count(area.width).min(7) as u16 + 1),
            Constraint::Length(match self.step {
                Step::Provider => 4,
                Step::Model => 3,
                Step::Credential => 5,
            }),
            Constraint::Length(4),
            Constraint::Min(0),
        ])
        .areas(area);
        frame.render_widget(header, header_area);
        match self.step {
            Step::Provider => {
                let lines: Vec<_> = PROVIDERS
                    .iter()
                    .enumerate()
                    .map(|(i, (_, label))| {
                        if i == self.provider {
                            Line::styled(format!("›  {label}"), palette.choice)
                        } else {
                            Line::raw(format!("   {label}"))
                        }
                    })
                    .collect();
                frame.render_widget(Paragraph::new(lines), body);
            }
            Step::Model | Step::Credential if body.width > 2 && body.height > 0 => {
                let (editor, placeholder, hint) = if matches!(self.step, Step::Model) {
                    (
                        &self.model,
                        "Model ID",
                        PROVIDERS[self.provider].1.to_owned(),
                    )
                } else {
                    (
                        &self.key_file,
                        "Path to API key file",
                        format!("Profile  {}", clean(&config.to_string_lossy())),
                    )
                };
                let [row, context] =
                    Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(body);
                let [marker, input] =
                    Layout::horizontal([Constraint::Length(2), Constraint::Min(0)]).areas(row);
                let managed = matches!(self.step, Step::Credential) && !self.use_key_file;
                let (value, x) = if managed {
                    let max = usize::from(input.width.saturating_sub(1));
                    let cells = self.api_key.chars().count().min(max);
                    let value = if self.api_key.is_empty() {
                        Line::styled("Paste API key", palette.quiet)
                    } else {
                        Line::raw("•".repeat(cells))
                    };
                    (value, cells as u16)
                } else {
                    let (lines, x, _) = editor.viewport(input.width, 1);
                    let value = if editor.text().is_empty() {
                        Line::styled(placeholder, palette.quiet)
                    } else {
                        Line::raw(lines.join(""))
                    };
                    (value, x)
                };
                frame.render_widget(Block::default().style(palette.rail), row);
                frame.render_widget(Line::styled("›", palette.label), marker);
                frame.render_widget(value, input);
                frame.set_cursor_position(Position::new(input.x + x, input.y));
                frame.render_widget(
                    Paragraph::new(hint)
                        .style(palette.quiet)
                        .wrap(Wrap { trim: false }),
                    context,
                );
            }
            _ => {}
        }
        let footer = Paragraph::new(vec![
            Line::styled(help, palette.quiet),
            Line::styled(clean(&self.error), palette.status(Color::Red)),
        ])
        .wrap(Wrap { trim: false });
        frame.render_widget(footer, footer_area);
    }
}
