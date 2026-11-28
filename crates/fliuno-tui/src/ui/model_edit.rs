use super::{clean, palette::Palette, render_list, setup_catalog::Catalog, step};
use crate::{editor::Editor, service::Request};
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use fliuno_protocol::{
    ModelProfileEditParams, ModelProfileEditView, ProviderModelListParams, ProviderModelPage,
};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Position, Rect},
    style::Color,
    text::Line,
    widgets::{Block, Paragraph, Wrap},
};

const MODEL_LIMIT: usize = 256;

#[derive(Default, PartialEq, Eq)]
enum Mode {
    #[default]
    Fields,
    Catalog,
    Effort,
}

pub(super) struct Panel {
    view: ModelProfileEditView,
    model: Editor,
    effort: Option<String>,
    effort_index: usize,
    mode: Mode,
    search: Editor,
    catalog: Catalog,
    error: Option<String>,
}

impl Panel {
    pub fn new(view: ModelProfileEditView) -> Self {
        let mut model = Editor::default();
        let _ = model.insert(&clean(&view.profile.model).replace('\n', " "), MODEL_LIMIT);
        Self {
            effort: view.profile.reasoning_effort.clone(),
            view,
            model,
            effort_index: 0,
            mode: Mode::Fields,
            search: Editor::default(),
            catalog: Catalog::default(),
            error: None,
        }
    }

    pub fn title(&self) -> &'static str {
        match self.mode {
            Mode::Fields => "Edit model profile",
            Mode::Catalog => "Browse provider models",
            Mode::Effort => "Set reasoning effort",
        }
    }

    pub fn preferred_height(&self) -> u16 {
        match self.mode {
            Mode::Fields => {
                if self.error.is_some() {
                    13
                } else {
                    9
                }
            }
            Mode::Catalog => (7 + self.catalog.visible_rows(self.search.text())).clamp(9, 15),
            Mode::Effort => 13,
        }
    }

    pub fn failed(&mut self, error: String) {
        self.error = Some(error);
    }

    pub fn install_page(&mut self, page: ProviderModelPage) {
        self.catalog.install_page(page);
    }

    /// True when Esc should close the whole editor instead of returning to the form.
    pub fn back(&mut self) -> bool {
        if self.mode == Mode::Fields {
            return true;
        }
        self.mode = Mode::Fields;
        self.error = None;
        false
    }

    pub fn paste(&mut self, text: &str, busy: bool) {
        if text.chars().any(char::is_control) {
            return;
        }
        let changed = match self.mode {
            Mode::Fields if !busy => self.model.insert(text, MODEL_LIMIT),
            Mode::Catalog => {
                let changed = self.search.insert(text, MODEL_LIMIT);
                if changed {
                    self.catalog.reset_selection();
                }
                changed
            }
            _ => false,
        };
        if changed && self.mode != Mode::Catalog {
            self.error = None;
        }
    }

    fn browse(&mut self, cursor: Option<String>) -> Request {
        self.error = None;
        self.catalog.page_requested(cursor.clone());
        Request::BrowseModels(ProviderModelListParams {
            profile: self.view.profile.profile.clone(),
            base_profile: self.view.profile.profile.is_none(),
            revision: Some(self.view.revision.clone()),
            cursor,
        })
    }

    pub fn key(&mut self, key: KeyEvent, busy: bool) -> Option<Request> {
        let pressed = key.kind == KeyEventKind::Press;
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if self.mode == Mode::Catalog {
            match key.code {
                KeyCode::Enter | KeyCode::Tab if pressed => {
                    if let Some(id) = self.catalog.selected(self.search.text()) {
                        self.model.clear();
                        let _ = self.model.insert(id, MODEL_LIMIT);
                        self.mode = Mode::Fields;
                        self.error = None;
                    }
                }
                KeyCode::Up | KeyCode::Down | KeyCode::PageUp | KeyCode::PageDown => {
                    self.catalog.navigate(key.code, self.search.text());
                }
                KeyCode::F(5) if pressed && !busy => return Some(self.browse(None)),
                KeyCode::F(6) if pressed && !busy => {
                    let cursor = self.catalog.cursor()?.to_owned();
                    return Some(self.browse(Some(cursor)));
                }
                KeyCode::Char(ch) if !ctrl && !key.modifiers.contains(KeyModifiers::ALT) => {
                    self.paste(ch.encode_utf8(&mut [0; 4]), busy);
                }
                code => {
                    if self.search.key(code) {
                        self.catalog.reset_selection();
                    }
                }
            }
            return None;
        }
        if busy {
            return None;
        }
        if self.mode == Mode::Effort {
            if pressed && matches!(key.code, KeyCode::Enter | KeyCode::Tab) {
                self.effort = self
                    .effort_index
                    .checked_sub(1)
                    .and_then(|idx| self.view.configurable_efforts.get(idx))
                    .cloned();
                self.mode = Mode::Fields;
            } else {
                step(
                    key.code,
                    &mut self.effort_index,
                    self.view.configurable_efforts.len() + 1,
                );
            }
            return None;
        }
        match key.code {
            KeyCode::Char('s') if ctrl && pressed => {
                if self.model.text().trim().is_empty() {
                    self.error = Some("Enter a model ID.".into());
                    return None;
                }
                self.error = None;
                return Some(Request::SaveModelEdit(ModelProfileEditParams {
                    profile: self.view.profile.profile.clone(),
                    revision: self.view.revision.clone(),
                    model: self.model.text().to_owned(),
                    reasoning_effort: self.effort.clone(),
                }));
            }
            KeyCode::F(5) if pressed => {
                self.mode = Mode::Catalog;
                self.search.clear();
                return Some(self.browse(None));
            }
            KeyCode::F(3) if pressed => {
                self.effort_index = self
                    .effort
                    .as_ref()
                    .and_then(|selected| {
                        self.view
                            .configurable_efforts
                            .iter()
                            .position(|effort| effort == selected)
                    })
                    .map_or(0, |idx| idx + 1);
                self.mode = Mode::Effort;
            }
            KeyCode::Char(ch) if !ctrl && !key.modifiers.contains(KeyModifiers::ALT) => {
                self.paste(ch.encode_utf8(&mut [0; 4]), busy);
            }
            code => {
                self.model.key(code);
            }
        }
        None
    }

    pub fn draw(&self, frame: &mut Frame<'_>, body: Rect, palette: &Palette) {
        let help = match self.mode {
            Mode::Fields => "F5 Browse   F3 Reasoning   Ctrl+S Save",
            Mode::Catalog if self.catalog.cursor().is_some() => "Enter Pick   F5 Reload   F6 More",
            Mode::Catalog => "Enter Pick   F5 Reload",
            Mode::Effort => "↑/↓ Move   Enter Apply",
        };
        let mut lines = vec![Line::styled(help, palette.quiet)];
        if let Some(error) = &self.error {
            lines.push(Line::styled(clean(error), palette.status(Color::Red)));
        }
        let footer = Paragraph::new(lines).wrap(Wrap { trim: true });
        let footer_height =
            (footer.line_count(body.width) as u16).min(body.height.saturating_sub(1));
        let [main, hint] =
            Layout::vertical([Constraint::Min(1), Constraint::Length(footer_height)]).areas(body);
        frame.render_widget(footer, hint);
        match self.mode {
            Mode::Fields => {
                let [header, input, fields] = Layout::vertical([
                    Constraint::Length(2),
                    Constraint::Length(1),
                    Constraint::Min(0),
                ])
                .areas(main);
                let label = clean(self.view.profile.profile.as_deref().unwrap_or("[model]"));
                let provider = clean(&self.view.profile.provider);
                frame.render_widget(
                    Paragraph::new(vec![
                        Line::styled(format!("{label}  ·  {provider}"), palette.quiet),
                        Line::raw("Model ID"),
                    ]),
                    header,
                );
                draw_field(frame, input, &self.model, "Model ID", palette);
                let effort = self.effort.as_deref().unwrap_or("provider default");
                frame.render_widget(
                    Paragraph::new(format!("Reasoning: {effort}")).wrap(Wrap { trim: true }),
                    fields,
                );
            }
            Mode::Catalog => {
                let status = if self.error.is_some() {
                    ""
                } else {
                    self.catalog.status(self.search.text())
                };
                let has_status = !status.is_empty();
                let status = Paragraph::new(status)
                    .style(palette.quiet)
                    .wrap(Wrap { trim: true });
                let status_rows = if has_status {
                    status.line_count(main.width).min(2) as u16
                } else {
                    0
                };
                let [input, status_area, list] = Layout::vertical([
                    Constraint::Length(1),
                    Constraint::Length(status_rows),
                    Constraint::Min(0),
                ])
                .areas(main);
                draw_field(frame, input, &self.search, "Search model IDs", palette);
                frame.render_widget(status, status_area);
                self.catalog
                    .draw(frame, list, self.search.text(), palette.choice);
            }
            Mode::Effort => {
                let levels = std::iter::once("Provider default")
                    .chain(self.view.configurable_efforts.iter().map(String::as_str))
                    .map(str::to_owned)
                    .collect();
                render_list(frame, main, levels, self.effort_index, palette.choice);
            }
        }
    }
}

fn draw_field(
    frame: &mut Frame<'_>,
    area: Rect,
    editor: &Editor,
    placeholder: &str,
    palette: &Palette,
) {
    if area.width <= 2 || area.height == 0 {
        return;
    }
    let [marker, input] =
        Layout::horizontal([Constraint::Length(2), Constraint::Min(0)]).areas(area);
    let (lines, x, _) = editor.viewport(input.width, 1);
    let line = if editor.text().is_empty() {
        Line::styled(placeholder, palette.quiet)
    } else {
        Line::raw(lines.join(""))
    };
    frame.render_widget(Block::default().style(palette.rail), area);
    frame.render_widget(Line::styled("›", palette.label), marker);
    frame.render_widget(line, input);
    frame.set_cursor_position(Position::new(input.x + x, input.y));
}
