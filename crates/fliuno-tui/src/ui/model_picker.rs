use super::*;
use fliuno_protocol::{
    ModelCatalogAccess, ModelCatalogStatus, ModelConfigurationStatus, ModelConnectionCheck,
    ModelConnectionCheckParams, ModelCredentialsStatus, ModelPresence, ModelProfileList,
    ModelProfileSelectParams,
};

pub(super) struct Picker {
    origin: CommandOrigin,
    profiles: Option<ModelProfileList>,
    index: usize,
    error: Option<String>,
    pending: Option<Option<String>>,
    checked: Option<(Option<String>, ModelConnectionCheck)>,
}

impl Picker {
    pub(super) fn new(origin: CommandOrigin) -> Self {
        Self {
            origin,
            profiles: None,
            index: 0,
            error: None,
            pending: None,
            checked: None,
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

    pub(super) fn checked(&mut self, profile: Option<String>, check: ModelConnectionCheck) {
        self.pending = None;
        self.checked = Some((profile, check));
    }

    pub(super) fn failed(&mut self, error: String) {
        self.pending = None;
        self.error = Some(error);
    }

    fn status(&self) -> Option<String> {
        let profile = &self.profiles.as_ref()?.profiles.get(self.index)?.profile;
        if self.pending.as_ref() == Some(profile) {
            return Some("Checking the model connection…".into());
        }
        let (checked, status) = self.checked.as_ref()?;
        (checked == profile).then(|| describe_check(status))
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
        (models as u16
            + if self.error.is_some() || self.status().is_some() {
                7
            } else {
                5
            })
        .max(7)
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
        let mut lines = vec![Line::styled(
            "Enter Use for now   F5 Check",
            palette.quiet,
        )];
        if let Some(error) = &self.error {
            lines.push(Line::styled(error.as_str(), palette.status(Color::Red)));
        } else if let Some(status) = self.status() {
            lines.push(Line::styled(status, palette.label));
        }
        let hint = Paragraph::new(lines).wrap(Wrap { trim: true });
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
        if key.code == KeyCode::Esc && (!self.busy || picker.pending.is_some()) {
            self.layer = picker.return_layer();
            return;
        }
        if self.busy && picker.pending.is_none() {
            return;
        }
        if let Some(profiles) = &picker.profiles {
            let before = picker.index;
            step(key.code, &mut picker.index, profiles.profiles.len());
            if before != picker.index {
                picker.error = None;
            }
            if self.busy || key.kind != KeyEventKind::Press {
                return;
            }
            let Some(profile) = profiles.profiles.get(picker.index) else {
                return;
            };
            if key.code == KeyCode::F(5) {
                picker.error = None;
                let id = profile.profile.clone();
                let params = ModelConnectionCheckParams {
                    base_profile: id.is_none(),
                    profile: id.clone(),
                };
                let sent = self.send(worker, Request::CheckModel(params));
                if let Layer::ModelPicker(picker) = &mut self.layer {
                    if sent {
                        picker.pending = Some(id);
                    } else {
                        picker.failed("Connection worker unavailable".into());
                    }
                }
                return;
            }
            if key.code == KeyCode::Enter {
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

fn describe_check(check: &ModelConnectionCheck) -> String {
    if check.configuration == ModelConfigurationStatus::Invalid {
        return check
            .configuration_issue
            .as_deref()
            .map(clean)
            .unwrap_or_else(|| "Invalid model configuration".into());
    }
    if check.credentials == ModelCredentialsStatus::Unavailable {
        return "Cannot read the configured API key.".into();
    }
    if check.catalog_access == ModelCatalogAccess::Unauthorized {
        return "API key rejected by the model catalog.".into();
    }
    if check.catalog_access == ModelCatalogAccess::Forbidden {
        return "The API key cannot access the model catalog.".into();
    }
    match (check.catalog, check.model_presence) {
        (ModelCatalogStatus::Available, ModelPresence::Present) => {
            "Catalog reachable. This model is listed."
        }
        (ModelCatalogStatus::Available, ModelPresence::NotListed) => {
            "Catalog reachable. This model was not listed."
        }
        (ModelCatalogStatus::Available, ModelPresence::Incomplete) => {
            "Catalog reachable. Model presence could not be confirmed."
        }
        (ModelCatalogStatus::Available, _) => "Catalog reachable. No model ID configured.",
        (ModelCatalogStatus::Unavailable, _) => "This endpoint does not expose a model catalog.",
        (ModelCatalogStatus::TimedOut, _) => "Model catalog timed out.",
        (ModelCatalogStatus::InvalidResponse, _) => {
            "The model catalog returned an invalid response."
        }
        _ => "Could not reach the model catalog.",
    }
    .into()
}
