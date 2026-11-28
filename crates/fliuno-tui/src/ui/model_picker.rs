use super::*;
use fliuno_protocol::{
    ModelCatalogAccess, ModelCatalogPreview, ModelCatalogStatus, ModelConfigurationStatus,
    ModelConnectionCheck, ModelConnectionCheckParams, ModelCredentialsStatus, ModelPresence,
    ModelProfileConnectionGetParams, ModelProfileConnectionTarget, ModelProfileConnectionView,
    ModelProfileDefault, ModelProfileEditGetParams, ModelProfileEditView, ModelProfileEntry,
    ModelProfileList, ModelProfileSelectParams,
};

pub(super) struct Picker {
    origin: CommandOrigin,
    profiles: Option<ModelProfileList>,
    default: Option<ModelProfileDefault>,
    saved: bool,
    editable: bool,
    connection_editable: bool,
    connection: Option<Box<super::model_connection::Panel>>,
    editor: Option<super::model_edit::Panel>,
    edit_saved: bool,
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
            default: None,
            saved: false,
            editable: false,
            connection_editable: false,
            connection: None,
            editor: None,
            edit_saved: false,
            index: 0,
            error: None,
            pending: None,
            checked: None,
        }
    }

    pub(super) fn install(
        &mut self,
        profiles: ModelProfileList,
        default: Option<ModelProfileDefault>,
        features: crate::service::ModelProfileFeatures,
    ) {
        self.default = default;
        self.editable = features.edit;
        self.connection_editable = features.connection;
        self.index = profiles
            .profiles
            .iter()
            .position(|entry| entry.profile == profiles.active.profile)
            .unwrap_or(0);
        self.profiles = Some(profiles);
        self.error = None;
    }

    pub(super) fn connection_opened(
        &mut self,
        target: ModelProfileConnectionTarget,
        view: ModelProfileConnectionView,
    ) {
        let panel = self
            .connection
            .get_or_insert_with(|| Box::new(super::model_connection::Panel::new()));
        panel.opened(target, view);
    }

    pub(super) fn connection_previewed(&mut self, preview: ModelCatalogPreview) {
        if let Some(panel) = &mut self.connection {
            panel.install_preview(preview);
        }
    }

    pub(super) fn connection_saved(&mut self, entry: ModelProfileEntry) {
        if let Some(profiles) = &mut self.profiles {
            if let Some(index) = profiles
                .profiles
                .iter()
                .position(|profile| profile.profile == entry.profile)
            {
                profiles.profiles[index] = entry;
                self.index = index;
            } else {
                self.index = profiles.profiles.len();
                profiles.profiles.push(entry);
            }
        }
        self.checked = None;
        self.connection = None;
        self.edit_saved = true;
    }

    pub(super) fn edit_opened(&mut self, view: ModelProfileEditView) {
        self.editor = Some(super::model_edit::Panel::new(view));
    }

    pub(super) fn edit_saved(&mut self, view: ModelProfileEditView) {
        if let Some(entry) = self
            .profiles
            .as_mut()
            .and_then(|profiles| profiles.profiles.get_mut(self.index))
        {
            *entry = view.profile;
        }
        self.checked = None;
        self.editor = None;
        self.edit_saved = true;
    }

    pub(super) fn catalog_page(&mut self, page: fliuno_protocol::ProviderModelPage) {
        if let Some(editor) = &mut self.editor {
            editor.install_page(page);
        }
    }

    pub(super) fn paste(&mut self, value: &str, busy: bool) {
        if let Some(panel) = &mut self.connection {
            panel.paste(value, busy);
        } else if let Some(editor) = &mut self.editor {
            editor.paste(value, busy);
        }
    }

    pub(super) fn title(&self) -> &'static str {
        if let Some(panel) = &self.connection {
            return panel.title();
        }
        self.editor
            .as_ref()
            .map_or("Switch model", super::model_edit::Panel::title)
    }

    pub(super) fn actions(&self) -> &'static str {
        if self.editor.is_some() || self.connection.is_some() {
            "Esc Back"
        } else {
            "↑/↓ Move  ·  Esc back"
        }
    }

    pub(super) fn saved(&mut self, default: ModelProfileDefault) {
        self.default = Some(default);
        self.saved = true;
    }

    pub(super) fn checked(&mut self, profile: Option<String>, check: ModelConnectionCheck) {
        self.pending = None;
        self.checked = Some((profile, check));
    }

    pub(super) fn failed(&mut self, error: String) {
        if let Some(panel) = &mut self.connection {
            panel.failed(error);
            return;
        }
        if let Some(editor) = &mut self.editor {
            editor.failed(error);
            return;
        }
        self.pending = None;
        self.error = Some(error);
        self.saved = false;
    }

    fn status(&self) -> Option<String> {
        if self.edit_saved {
            return Some("Profile saved. Enter to use it now.".into());
        }
        if self.saved {
            return Some("Startup default saved. Active model unchanged.".into());
        }
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

    pub(super) fn editing_connection(&self) -> bool {
        self.connection.is_some()
    }

    pub(super) fn preferred_height(&self) -> u16 {
        if let Some(panel) = &self.connection {
            return panel.preferred_height();
        }
        if let Some(editor) = &self.editor {
            return editor.preferred_height();
        }
        let models = self
            .profiles
            .as_ref()
            .map_or(1, |list| list.profiles.len().clamp(1, 8));
        (models as u16
            + u16::from(self.editable)
            + u16::from(self.connection_editable)
            + if self.error.is_some() || self.status().is_some() {
                7
            } else {
                5
            })
        .max(7)
    }

    pub(super) fn draw(&self, frame: &mut Frame<'_>, body: Rect, palette: &Palette) {
        if let Some(panel) = &self.connection {
            panel.draw(frame, body, palette);
            return;
        }
        if let Some(editor) = &self.editor {
            editor.draw(frame, body, palette);
            return;
        }
        let Some(profiles) = &self.profiles else {
            let text = self.error.as_deref().unwrap_or("Loading model profiles…");
            render_text(frame, body, text, 0);
            return;
        };
        if profiles.profiles.is_empty() {
            let help = if self.connection_editable {
                "No model profiles. Ctrl+N Add profile"
            } else {
                "Configure a live model in config.toml."
            };
            render_text(frame, body, help, 0);
            return;
        }
        let help = if self.default.is_some() {
            "Enter Use now   F5 Check   Ctrl+S Default"
        } else {
            "Enter Use now   F5 Check"
        };
        let mut lines = vec![Line::styled(help, palette.quiet)];
        if self.editable {
            lines.push(Line::styled("F2 Edit profile", palette.quiet));
        }
        if self.connection_editable {
            lines.push(Line::styled(
                "F4 Connection   Ctrl+N Add profile",
                palette.quiet,
            ));
        }
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
                let active = profiles.active.profile == entry.profile;
                let default = self
                    .default
                    .as_ref()
                    .is_some_and(|default| default.profile == entry.profile);
                let pending = active && *entry != profiles.active;
                let role = match (active && !pending, default) {
                    (true, true) => " [now/default]",
                    (true, false) => " [now]",
                    (false, true) => " [default]",
                    _ => "",
                };
                let model = clean(&entry.model).replace('\n', " ");
                let effort = entry
                    .reasoning_effort
                    .as_deref()
                    .map_or_else(String::new, |effort| format!("  ·  {}", clean(effort)));
                // Keep the model ID visible on narrow terminals; the profile state still leads.
                let pending = if pending { " [saved]" } else { "" };
                format!("{id}{role}{pending}  ·  {model}{effort}")
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
        if let Some(panel) = &mut picker.connection {
            if key.code == KeyCode::Esc {
                if !self.busy && panel.back() {
                    picker.connection = None;
                }
            } else if let Some(request) = panel.key(key, self.busy)
                && !self.send(worker, request)
                && let Layer::ModelPicker(picker) = &mut self.layer
            {
                picker.failed("Connection worker unavailable".into());
            }
            return;
        }
        if let Some(editor) = &mut picker.editor {
            if key.code == KeyCode::Esc {
                if !self.busy && editor.back() {
                    picker.editor = None;
                }
            } else if let Some(request) = editor.key(key, self.busy)
                && !self.send(worker, request)
                && let Layer::ModelPicker(picker) = &mut self.layer
            {
                picker.failed("Connection worker unavailable".into());
            }
            return;
        }
        if key.code == KeyCode::Esc && (!self.busy || picker.pending.is_some()) {
            self.layer = picker.return_layer();
            return;
        }
        if self.busy && picker.pending.is_none() {
            return;
        }
        if !self.busy
            && picker.connection_editable
            && key.kind == KeyEventKind::Press
            && key.code == KeyCode::Char('n')
            && key.modifiers.contains(KeyModifiers::CONTROL)
        {
            picker.connection = Some(Box::new(super::model_connection::Panel::new()));
            picker.error = None;
            picker.edit_saved = false;
            picker.saved = false;
            return;
        }
        if let Some(profiles) = &picker.profiles {
            let before = picker.index;
            step(key.code, &mut picker.index, profiles.profiles.len());
            if before != picker.index {
                picker.error = None;
                picker.saved = false;
                picker.edit_saved = false;
            }
            if self.busy || key.kind != KeyEventKind::Press {
                return;
            }
            let Some(profile) = profiles.profiles.get(picker.index) else {
                return;
            };
            if key.code == KeyCode::F(4) && picker.connection_editable {
                picker.error = None;
                picker.saved = false;
                picker.edit_saved = false;
                let target = profile
                    .profile
                    .clone()
                    .map_or(ModelProfileConnectionTarget::Base, |profile| {
                        ModelProfileConnectionTarget::Existing { profile }
                    });
                if !self.send(
                    worker,
                    Request::OpenModelConnection(ModelProfileConnectionGetParams { target }),
                ) && let Layer::ModelPicker(picker) = &mut self.layer
                {
                    picker.failed("Connection worker unavailable".into());
                }
                return;
            }
            if key.code == KeyCode::F(2) && picker.editable {
                picker.error = None;
                picker.saved = false;
                picker.edit_saved = false;
                let params = ModelProfileEditGetParams {
                    profile: profile.profile.clone(),
                };
                if !self.send(worker, Request::EditModel(params))
                    && let Layer::ModelPicker(picker) = &mut self.layer
                {
                    picker.failed("Connection worker unavailable".into());
                }
                return;
            }
            if key.code == KeyCode::Char('s')
                && key.modifiers.contains(KeyModifiers::CONTROL)
                && picker.default.is_some()
            {
                picker.error = None;
                picker.saved = false;
                let default = ModelProfileDefault {
                    profile: profile.profile.clone(),
                };
                if !self.send(worker, Request::SaveModelDefault(default))
                    && let Layer::ModelPicker(picker) = &mut self.layer
                {
                    picker.failed("Connection worker unavailable".into());
                }
                return;
            }
            if key.code == KeyCode::F(5) {
                picker.error = None;
                picker.saved = false;
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
                    reload: profile.profile == profiles.active.profile,
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
