use super::*;
use fliuno_protocol::{ModelProfileList, ModelProfileSelectParams};

pub(super) struct Picker {
    origin: CommandOrigin,
    profiles: Option<ModelProfileList>,
    index: usize,
    pub(super) error: Option<String>,
}

impl Picker {
    pub(super) fn new(origin: CommandOrigin) -> Self {
        Self {
            origin,
            profiles: None,
            index: 0,
            error: None,
        }
    }

    pub(super) fn install(&mut self, profiles: ModelProfileList) {
        self.index = profiles
            .profiles
            .iter()
            .position(|entry| entry.profile == profiles.active.profile)
            .unwrap_or(0);
        self.profiles = Some(profiles);
        self.error = None;
    }

    pub(super) fn return_layer(&self) -> Layer {
        if self.origin == CommandOrigin::Workbench {
            Layer::None
        } else {
            Layer::Edit(Edit::Task)
        }
    }

    pub(super) fn preferred_height(&self) -> u16 {
        let models = self
            .profiles
            .as_ref()
            .map_or(1, |list| list.profiles.len().clamp(1, 8));
        (models as u16 + if self.error.is_some() { 7 } else { 5 }).max(6)
    }

    pub(super) fn draw(&self, frame: &mut Frame<'_>, body: Rect, palette: &Palette) {
        let Some(profiles) = &self.profiles else {
            let text = self.error.as_deref().unwrap_or("Loading model profiles…");
            render_text(frame, body, text, 0);
            return;
        };
        if profiles.profiles.is_empty() {
            render_text(frame, body, "Configure a live model in config.toml.", 0);
            return;
        }
        let hint = if let Some(error) = &self.error {
            error.as_str()
        } else {
            "Applies until reconnect or exit."
        };
        let style = if self.error.is_some() {
            palette.label
        } else {
            palette.quiet
        };
        let hint = Paragraph::new(hint).style(style).wrap(Wrap { trim: true });
        let hint_height = (hint.line_count(body.width) as u16).min(body.height.saturating_sub(1));
        let [list, footer] =
            Layout::vertical([Constraint::Min(1), Constraint::Length(hint_height)]).areas(body);
        let labels = profiles
            .profiles
            .iter()
            .map(|entry| {
                let id = entry.profile.as_deref().unwrap_or("[model]");
                let active = if profiles.active.profile == entry.profile {
                    " (active)"
                } else {
                    ""
                };
                let model = clean(&entry.model).replace('\n', " ");
                let effort = entry
                    .reasoning_effort
                    .as_deref()
                    .map_or_else(String::new, |effort| format!("  ·  {}", clean(effort)));
                format!("{id}{active}  ·  {} / {model}{effort}", entry.provider)
            })
            .collect();
        render_list(frame, list, labels, self.index, palette.choice);
        frame.render_widget(hint, footer);
    }
}

impl App {
    pub(super) fn model_picker_key(&mut self, key: KeyEvent, worker: &Worker) {
        let Layer::ModelPicker(picker) = &mut self.layer else {
            return;
        };
        if self.busy {
            return;
        }
        if key.code == KeyCode::Esc {
            self.layer = picker.return_layer();
            return;
        }
        if let Some(profiles) = &picker.profiles {
            step(key.code, &mut picker.index, profiles.profiles.len());
            if key.code == KeyCode::Enter
                && key.kind == KeyEventKind::Press
                && let Some(profile) = profiles.profiles.get(picker.index)
            {
                picker.error = None;
                let params = ModelProfileSelectParams {
                    profile: profile.profile.clone(),
                };
                if !self.send(worker, Request::SelectModel(params))
                    && let Layer::ModelPicker(picker) = &mut self.layer
                {
                    picker.error = Some("Connection worker unavailable".into());
                }
            }
        }
    }
}
