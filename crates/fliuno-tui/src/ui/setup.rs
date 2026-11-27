use super::{clean, palette::Palette};
use crate::{Screen, editor::Editor, options::SetupOptions};
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use fliuno_config::{Provider, read_api_key};
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
    KeyFile,
}

#[derive(Default)]
struct Form {
    step: Step,
    provider: usize,
    model: Editor,
    key_file: Editor,
    error: String,
}

pub fn run(screen: &mut Screen, options: SetupOptions) -> io::Result<bool> {
    let mut form = Form::default();
    let palette = Palette::new(options.theme);
    loop {
        screen.draw(|frame| form.draw(frame, &options.config, &palette))?;
        match event::read()? {
            Event::Paste(text) => form.insert(&text),
            Event::Key(key) if key.kind != KeyEventKind::Release => {
                let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
                match key.code {
                    KeyCode::Char('c') if ctrl => return Ok(false),
                    KeyCode::Esc => {
                        form.error.clear();
                        form.step = match form.step {
                            Step::Provider => return Ok(false),
                            Step::Model => Step::Provider,
                            Step::KeyFile => Step::Model,
                        };
                    }
                    KeyCode::Enter if key.kind == KeyEventKind::Press => {
                        form.error.clear();
                        match form.step {
                            Step::Provider => form.step = Step::Model,
                            Step::Model if form.model.text().trim().is_empty() => {
                                form.error = "Enter a model ID available on your account.".into();
                            }
                            Step::Model => form.step = Step::KeyFile,
                            Step::KeyFile => match form.save(&options.config) {
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
            Step::Provider => None,
            Step::Model => Some(&mut self.model),
            Step::KeyFile => Some(&mut self.key_file),
        }
    }

    fn insert(&mut self, text: &str) {
        let limit = if matches!(self.step, Step::Model) {
            256
        } else {
            4096
        };
        let text = text.trim_matches(['\r', '\n']);
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
        let key_file = self.key_file.text().trim();
        if key_file.is_empty() {
            return Err("Enter the path to your API key file.".into());
        }
        let parent = config
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        read_api_key(&parent.join(key_file))?;
        let provider = PROVIDERS[self.provider].0.name();
        // JSON and TOML use the same string escapes for these single-line, control-free inputs.
        let model = serde_json::to_string(self.model.text().trim()).map_err(|e| e.to_string())?;
        let file = serde_json::to_string(key_file).map_err(|e| e.to_string())?;
        let content =
            format!("[model]\nprovider = \"{provider}\"\nname = {model}\napi_key_file = {file}\n");
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
        })
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
            Step::KeyFile => (
                3,
                "Connect an API key file",
                "Enter an absolute path or one relative to the profile. Use a file with one API key.",
                "Enter Save and open   Esc Back",
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
                Step::KeyFile => 5,
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
            Step::Model | Step::KeyFile if body.width > 0 && body.height > 0 => {
                let (editor, hint) = if matches!(self.step, Step::Model) {
                    (&self.model, PROVIDERS[self.provider].1.to_owned())
                } else {
                    (
                        &self.key_file,
                        format!("Profile  {}", clean(&config.to_string_lossy())),
                    )
                };
                let [input, context] =
                    Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(body);
                let (lines, x, _) = editor.viewport(input.width, 1);
                frame.render_widget(Paragraph::new(lines.join("")).style(palette.rail), input);
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
