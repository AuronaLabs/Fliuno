use super::{clean, palette::Palette, setup_catalog::Catalog};
use crate::{Screen, editor::Editor, options::SetupOptions};
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use fliuno_config::{
    ConnectionCredential, ModelCatalogDraft, ModelConnectionDraft, ModelProtocol, ModelSetup,
    ModelSetupCredential, Provider, ReasoningEffort, Settings, is_local_provider_root,
    provider_api_root,
};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Margin, Position, Rect},
    style::Color,
    text::Line,
    widgets::{Block, Paragraph, Wrap},
};
use std::{io, path::Path, time::Duration};
use zeroize::{Zeroize, Zeroizing};

pub(super) struct ModelRoute {
    pub(super) provider: Provider,
    pub(super) protocol: ModelProtocol,
    pub(super) label: &'static str,
}

pub(super) const MODEL_ROUTES: [ModelRoute; 4] = [
    ModelRoute {
        provider: Provider::OpenAi,
        protocol: ModelProtocol::Responses,
        label: "OpenAI Responses",
    },
    ModelRoute {
        provider: Provider::OpenAi,
        protocol: ModelProtocol::ChatCompletions,
        label: "OpenAI-compatible Chat Completions",
    },
    ModelRoute {
        provider: Provider::Anthropic,
        protocol: ModelProtocol::Messages,
        label: "Anthropic Messages",
    },
    ModelRoute {
        provider: Provider::Gemini,
        protocol: ModelProtocol::Interactions,
        label: "Google Gemini Interactions",
    },
];

#[derive(Default)]
enum Step {
    #[default]
    Provider,
    Model,
    Endpoint,
    Credential,
    Reasoning,
}

#[derive(Default)]
struct Form {
    step: Step,
    catalog: Catalog,
    route: usize,
    effort: Option<ReasoningEffort>,
    effort_index: usize,
    model: Editor,
    endpoint: Editor,
    key_file: Editor,
    use_key_file: bool,
    keyless: bool,
    api_key: Zeroizing<String>,
    error: String,
}

pub fn run(screen: &mut Screen, options: SetupOptions) -> io::Result<bool> {
    let mut form = Form::default();
    let palette = Palette::new(options.theme);
    let mut redraw = true;
    loop {
        let was_loading = form.catalog.loading();
        form.catalog.poll();
        if redraw || (was_loading && !form.catalog.loading()) {
            screen.draw(|frame| form.draw(frame, &options.config, &palette))?;
            redraw = false;
        }
        if form.catalog.loading() && !event::poll(Duration::from_millis(50))? {
            continue;
        }
        redraw = true;
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
                        form.step = match form.step {
                            Step::Provider => return Ok(false),
                            Step::Reasoning => Step::Model,
                            Step::Endpoint => Step::Provider,
                            Step::Credential => {
                                form.api_key.zeroize();
                                Step::Endpoint
                            }
                            Step::Model => {
                                form.catalog = Catalog::default();
                                Step::Credential
                            }
                        };
                    }
                    KeyCode::F(2) if matches!(form.step, Step::Credential) => {
                        form.use_key_file = !form.keyless && !form.use_key_file;
                        form.keyless = false;
                        form.api_key.zeroize();
                        form.error.clear();
                    }
                    KeyCode::F(4)
                        if matches!(form.step, Step::Credential) && form.can_use_keyless() =>
                    {
                        form.keyless = !form.keyless;
                        form.use_key_file = false;
                        form.api_key.zeroize();
                        form.error.clear();
                    }
                    KeyCode::F(3) if matches!(form.step, Step::Model) => {
                        form.effort_index = form
                            .effort
                            .and_then(|selected| {
                                form.efforts().iter().position(|effort| *effort == selected)
                            })
                            .map_or(0, |index| index + 1);
                        form.error.clear();
                        form.step = Step::Reasoning;
                    }
                    KeyCode::F(5)
                        if matches!(form.step, Step::Model) && !form.catalog.loading() =>
                    {
                        form.refresh_catalog(&options, None);
                    }
                    KeyCode::F(6)
                        if matches!(form.step, Step::Model) && !form.catalog.loading() =>
                    {
                        if let Some(cursor) = form.catalog.cursor().map(str::to_owned) {
                            form.refresh_catalog(&options, Some(cursor));
                        }
                    }
                    KeyCode::Tab if matches!(form.step, Step::Model) => {
                        if let Some(id) =
                            form.catalog.selected(form.model.text()).map(str::to_owned)
                        {
                            form.model.clear();
                            form.insert(&id);
                        }
                    }
                    KeyCode::Enter if key.kind == KeyEventKind::Press => {
                        form.error.clear();
                        match form.step {
                            Step::Provider => {
                                if form
                                    .effort
                                    .is_some_and(|effort| !form.efforts().contains(&effort))
                                {
                                    form.effort = None;
                                }
                                form.step = Step::Endpoint;
                            }
                            Step::Reasoning => {
                                form.effort = form
                                    .effort_index
                                    .checked_sub(1)
                                    .and_then(|index| form.efforts().get(index).copied());
                                form.step = Step::Model;
                            }
                            Step::Endpoint => {
                                let endpoint = form.endpoint.text().trim();
                                if !endpoint.is_empty()
                                    && let Err(error) = provider_api_root(endpoint)
                                {
                                    form.error = error.into();
                                } else {
                                    form.keyless &= form.can_use_keyless();
                                    form.step = Step::Credential;
                                }
                            }
                            Step::Credential => {
                                if form.refresh_catalog(&options, None) {
                                    form.step = Step::Model;
                                }
                            }
                            Step::Model if form.model.text().trim().is_empty() => {
                                form.error = "Choose a catalog model or enter a model ID.".into();
                            }
                            Step::Model => match form.save(&options.config) {
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
    fn efforts(&self) -> &'static [ReasoningEffort] {
        MODEL_ROUTES[self.route].provider.reasoning_efforts()
    }

    fn can_use_keyless(&self) -> bool {
        MODEL_ROUTES[self.route].protocol == ModelProtocol::ChatCompletions
            && provider_api_root(self.endpoint.text().trim())
                .is_ok_and(|url| is_local_provider_root(&url))
    }

    fn refresh_catalog(&mut self, options: &SetupOptions, cursor: Option<String>) -> bool {
        let credential = if self.keyless {
            Ok(ConnectionCredential::None)
        } else if self.use_key_file {
            self.key_path().map(ConnectionCredential::File)
        } else if self.api_key.is_empty() {
            Err("Paste your API key or press F2 to use a key file.")
        } else {
            Ok(ConnectionCredential::Managed(&self.api_key))
        };
        let credential = match credential {
            Ok(credential) => credential,
            Err(error) => {
                self.error = error.into();
                return false;
            }
        };
        let route = &MODEL_ROUTES[self.route];
        let endpoint = self.endpoint.text().trim();
        let draft = ModelConnectionDraft {
            provider: route.provider,
            protocol: route.protocol,
            base_url: (!endpoint.is_empty()).then_some(endpoint),
            credential,
        };
        let (settings, key) = match draft.for_preview(&options.config, None) {
            Ok((settings, key)) => (settings, Zeroizing::new(key.unwrap_or_default())),
            Err(error) => {
                self.error = error;
                return false;
            }
        };
        self.error.clear();
        let draft = ModelCatalogDraft {
            provider: route.provider,
            protocol: route.protocol,
            base_url: settings.base_url,
            auth: settings.auth,
            cursor,
        };
        self.catalog.request(&options.server, draft, key);
        true
    }

    fn key_path(&self) -> Result<&str, &'static str> {
        let path = self.key_file.text().trim();
        if path.is_empty() {
            return Err("Enter the path to your API key file.");
        }
        Ok(path)
    }

    fn field(&mut self) -> Option<&mut Editor> {
        match self.step {
            Step::Model => Some(&mut self.model),
            Step::Endpoint => Some(&mut self.endpoint),
            Step::Credential if self.use_key_file => Some(&mut self.key_file),
            _ => None,
        }
    }

    fn insert(&mut self, text: &str) {
        if matches!(self.step, Step::Credential) && self.keyless {
            return;
        }
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
            self.catalog.reset_selection();
        }
    }

    fn navigate(&mut self, key: KeyCode) {
        if matches!(self.step, Step::Model) && matches!(key, KeyCode::Up | KeyCode::Down) {
            self.catalog.navigate(key, self.model.text());
        } else if let Some(field) = self.field() {
            if field.key(key) {
                self.error.clear();
                self.catalog.reset_selection();
            }
        } else if matches!(self.step, Step::Credential) {
            if key == KeyCode::Backspace {
                self.api_key.pop();
                self.error.clear();
            }
        } else if matches!(self.step, Step::Reasoning) {
            let len = self.efforts().len() + 1;
            super::step(key, &mut self.effort_index, len);
        } else {
            match key {
                KeyCode::Up | KeyCode::Left => self.route = self.route.saturating_sub(1),
                KeyCode::Down | KeyCode::Right => {
                    self.route = (self.route + 1).min(MODEL_ROUTES.len() - 1);
                }
                _ => {}
            }
        }
    }

    fn save(&self, config: &Path) -> Result<(), String> {
        let route = &MODEL_ROUTES[self.route];
        let endpoint = self.endpoint.text().trim();
        let credential = if self.keyless {
            ModelSetupCredential::None
        } else if self.use_key_file {
            ModelSetupCredential::File(self.key_path()?)
        } else {
            ModelSetupCredential::Managed(&self.api_key)
        };
        Settings::create_live_model(
            config,
            ModelSetup {
                provider: route.provider,
                protocol: route.protocol,
                model: self.model.text(),
                base_url: (!endpoint.is_empty()).then_some(endpoint),
                reasoning_effort: self.effort,
                credential,
            },
        )
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
                "Match the API offered by your provider or gateway.",
                "↑ ↓ Select   Enter Continue   Esc Exit",
            ),
            Step::Model => (
                4,
                "Choose a model",
                "Type a model ID or search the catalog.",
                "↑ ↓ Browse   Tab Use model   Enter Save ID",
            ),
            Step::Reasoning => (
                4,
                "Set reasoning effort",
                "Use the provider default unless you know your model supports a specific level.",
                "↑ ↓ Select   Enter Apply   Esc Back",
            ),
            Step::Endpoint => (
                2,
                "Set a Base URL",
                "Leave blank for the provider default, or enter your gateway's API root.",
                "Enter Continue   Esc Back",
            ),
            Step::Credential if self.keyless => (
                3,
                "Connect without a key",
                "Fliuno will connect directly to this computer. No API key will be sent.",
                "Enter Continue   F4 Use key   Esc Back",
            ),
            Step::Credential if self.use_key_file => (
                3,
                "Connect an API key file",
                "Enter an absolute path or one relative to the profile. Use a file with one API key.",
                "Enter Continue   F2 Paste key   Esc Back",
            ),
            Step::Credential => (
                3,
                "Connect your API key",
                if cfg!(windows) {
                    "Paste a key from your model provider. Windows Credential Manager stores it for your account."
                } else {
                    "Paste a key from your model provider. Fliuno stores it in a private file next to your profile."
                },
                if self.can_use_keyless() {
                    "Enter Continue   F2 Key file   F4 No key   Esc Back"
                } else {
                    "Enter Continue   F2 Key file   Esc Back"
                },
            ),
        };
        let header = Paragraph::new(vec![
            Line::styled(format!("Fliuno  /  Set up  ·  {step} of 4"), palette.label),
            Line::raw(""),
            Line::styled(heading, palette.label),
            Line::styled(description, palette.quiet),
        ])
        .wrap(Wrap { trim: false });
        let catalog_status = Paragraph::new(if matches!(self.step, Step::Model) {
            self.catalog.status(self.model.text())
        } else {
            ""
        })
        .style(palette.label)
        .wrap(Wrap { trim: false });
        let status_rows = if matches!(self.step, Step::Model)
            && !self.catalog.status(self.model.text()).is_empty()
        {
            catalog_status.line_count(area.width).min(3) as u16
        } else {
            0
        };
        let mut footer_lines = vec![Line::raw(""), Line::styled(help, palette.quiet)];
        if matches!(self.step, Step::Model) {
            let more = if self.catalog.cursor().is_some() {
                "   F6 More"
            } else {
                ""
            };
            footer_lines.push(Line::styled(
                format!("F5 Reload{more}   Esc Back"),
                palette.quiet,
            ));
            let effort = self
                .effort
                .map_or("provider default", ReasoningEffort::as_str);
            footer_lines.push(Line::styled(
                format!("F3 Reasoning: {effort}"),
                palette.quiet,
            ));
        }
        if !self.error.is_empty() {
            footer_lines.push(Line::styled(clean(&self.error), palette.status(Color::Red)));
        }
        let footer = Paragraph::new(footer_lines).wrap(Wrap { trim: false });
        let [header_area, body, footer_area, _] = Layout::vertical([
            Constraint::Length(header.line_count(area.width).min(7) as u16 + 1),
            Constraint::Length(match self.step {
                Step::Provider => MODEL_ROUTES.len() as u16,
                Step::Reasoning => (1 + self.efforts().len()) as u16,
                Step::Model => 1 + status_rows + self.catalog.visible_rows(self.model.text()),
                Step::Endpoint => 3,
                Step::Credential => 5,
            }),
            Constraint::Length(footer.line_count(area.width).min(7) as u16),
            Constraint::Min(0),
        ])
        .areas(area);
        frame.render_widget(header, header_area);
        match self.step {
            Step::Provider => {
                let lines: Vec<_> = MODEL_ROUTES
                    .iter()
                    .enumerate()
                    .map(|(i, route)| {
                        let label = route.label;
                        if i == self.route {
                            Line::styled(format!("›  {label}"), palette.choice)
                        } else {
                            Line::raw(format!("   {label}"))
                        }
                    })
                    .collect();
                frame.render_widget(Paragraph::new(lines), body);
            }
            Step::Reasoning => {
                let labels = std::iter::once("Provider default")
                    .chain(self.efforts().iter().map(|effort| effort.as_str()))
                    .map(str::to_owned)
                    .collect();
                super::render_list(frame, body, labels, self.effort_index, palette.choice);
            }
            Step::Credential if self.keyless => {
                frame.render_widget(
                    Paragraph::new(clean(self.endpoint.text().trim()))
                        .style(palette.label)
                        .wrap(Wrap { trim: false }),
                    body,
                );
            }
            Step::Model | Step::Endpoint | Step::Credential
                if body.width > 2 && body.height > 0 =>
            {
                let (editor, placeholder, hint) = match self.step {
                    Step::Model => (&self.model, "Model ID or search", String::new()),
                    Step::Endpoint => (&self.endpoint, "Provider default", String::new()),
                    _ => (
                        &self.key_file,
                        "Path to API key file",
                        format!("Profile  {}", clean(&config.to_string_lossy())),
                    ),
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
                if matches!(self.step, Step::Model) {
                    let [status, rows] =
                        Layout::vertical([Constraint::Length(status_rows), Constraint::Min(0)])
                            .areas(context);
                    frame.render_widget(catalog_status, status);
                    self.catalog
                        .draw(frame, rows, self.model.text(), palette.choice);
                } else {
                    frame.render_widget(
                        Paragraph::new(hint)
                            .style(palette.quiet)
                            .wrap(Wrap { trim: false }),
                        context,
                    );
                }
            }
            _ => {}
        }
        frame.render_widget(footer, footer_area);
    }
}
