use super::{model_edit::draw_field, setup::MODEL_ROUTES, setup_catalog::Catalog, *};
use fliuno_config::{ModelProtocol, is_local_provider_root, provider_api_root, validate_api_key};
use fliuno_protocol::{
    ModelCatalogPreview, ModelProfileConnectionDraft, ModelProfileConnectionGetParams,
    ModelProfileConnectionPreviewParams, ModelProfileConnectionSaveParams,
    ModelProfileConnectionTarget, ModelProfileConnectionView, ModelProfileCredentialInput,
    ModelProfileCredentialStatus,
};
use zeroize::{Zeroize, Zeroizing};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Step {
    Name,
    Protocol,
    Endpoint,
    Authentication,
    Secret,
    Model,
    Catalog,
    Effort,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Auth {
    Keep,
    Managed,
    File,
    None,
}

impl Auth {
    fn label(self) -> &'static str {
        match self {
            Self::Keep => "Keep current settings",
            Self::Managed => "Paste an API key",
            Self::File => "Use an API key file",
            Self::None => "Connect without a key",
        }
    }
}

pub(super) struct Panel {
    target: Option<ModelProfileConnectionTarget>,
    original: Option<ModelProfileConnectionView>,
    step: Step,
    name: Editor,
    endpoint: Editor,
    model: Editor,
    search: Editor,
    key_file: Editor,
    secret: Zeroizing<String>,
    route: usize,
    auth: Auth,
    effort: Option<String>,
    effort_index: usize,
    catalog: Catalog,
    error: Option<String>,
}

impl Panel {
    pub(super) fn new() -> Self {
        Self {
            target: None,
            original: None,
            step: Step::Name,
            name: Editor::default(),
            endpoint: Editor::default(),
            model: Editor::default(),
            search: Editor::default(),
            key_file: Editor::default(),
            secret: Zeroizing::new(String::new()),
            route: 0,
            auth: Auth::Managed,
            effort: None,
            effort_index: 0,
            catalog: Catalog::default(),
            error: None,
        }
    }

    pub(super) fn opened(
        &mut self,
        target: ModelProfileConnectionTarget,
        view: ModelProfileConnectionView,
    ) {
        self.route = MODEL_ROUTES
            .iter()
            .position(|route| {
                view.provider.as_deref() == Some(route.provider.name())
                    && view.protocol.as_deref() == Some(route.protocol.as_str())
            })
            .unwrap_or(0);
        self.endpoint.clear();
        self.model.clear();
        if let Some(url) = &view.base_url {
            let _ = self.endpoint.insert(url, 4096);
        }
        if let Some(model) = &view.model {
            let _ = self.model.insert(&clean(model).replace('\n', " "), 256);
        }
        self.effort = view.reasoning_effort.clone();
        self.target = Some(target);
        self.original = Some(view);
        self.auth = if self.can_keep() {
            Auth::Keep
        } else {
            Auth::Managed
        };
        self.error = None;
        self.step = Step::Protocol;
    }

    fn endpoint(&self) -> Option<&str> {
        let value = self.endpoint.text().trim();
        (!value.is_empty()).then_some(value)
    }

    fn can_keep(&self) -> bool {
        let Some(view) = &self.original else {
            return false;
        };
        let route = &MODEL_ROUTES[self.route];
        view.credential != ModelProfileCredentialStatus::Unconfigured
            && view.provider.as_deref() == Some(route.provider.name())
            && view.protocol.as_deref() == Some(route.protocol.as_str())
            && provider_api_root(route.provider.api_root(self.endpoint())).is_ok_and(|next| {
                provider_api_root(route.provider.api_root(view.base_url.as_deref()))
                    .is_ok_and(|old| next == old)
            })
    }

    fn auth_options(&self) -> Vec<Auth> {
        let mut items = Vec::with_capacity(4);
        if self.can_keep() {
            items.push(Auth::Keep);
        }
        items.extend([Auth::Managed, Auth::File]);
        if MODEL_ROUTES[self.route].protocol == ModelProtocol::ChatCompletions
            && self
                .endpoint()
                .and_then(|url| provider_api_root(url).ok())
                .is_some_and(|url| is_local_provider_root(&url))
        {
            items.push(Auth::None);
        }
        items
    }

    fn connection(&self) -> ModelProfileConnectionDraft {
        let route = &MODEL_ROUTES[self.route];
        ModelProfileConnectionDraft {
            provider: route.provider.name().into(),
            protocol: route.protocol.as_str().into(),
            base_url: self.endpoint().map(str::to_owned),
            credential: match self.auth {
                Auth::Keep => ModelProfileCredentialInput::Keep,
                Auth::Managed => ModelProfileCredentialInput::Managed {
                    key: self.secret.to_string(),
                },
                Auth::File => ModelProfileCredentialInput::File {
                    path: self.key_file.text().trim().to_owned(),
                },
                Auth::None => ModelProfileCredentialInput::None,
            },
        }
    }

    fn browse(&mut self, cursor: Option<String>) -> Option<Request> {
        let target = self.target.clone()?;
        let revision = self.original.as_ref()?.revision.clone();
        self.error = None;
        self.catalog.page_requested(cursor.clone());
        Some(Request::PreviewModelConnection(
            ModelProfileConnectionPreviewParams {
                target,
                revision,
                connection: self.connection(),
                cursor,
            },
        ))
    }

    pub(super) fn install_preview(&mut self, preview: ModelCatalogPreview) {
        self.catalog.install_preview(preview);
    }

    pub(super) fn failed(&mut self, error: String) {
        self.error = Some(error);
    }

    pub(super) fn back(&mut self) -> bool {
        self.error = None;
        self.step = match self.step {
            Step::Name | Step::Protocol => return true,
            Step::Endpoint => Step::Protocol,
            Step::Authentication => Step::Endpoint,
            Step::Secret => Step::Authentication,
            Step::Model if matches!(self.auth, Auth::Managed | Auth::File) => Step::Secret,
            Step::Model => Step::Authentication,
            Step::Catalog | Step::Effort => Step::Model,
        };
        false
    }

    fn editor(&mut self) -> Option<(&mut Editor, usize)> {
        match self.step {
            Step::Name => Some((&mut self.name, 48)),
            Step::Endpoint => Some((&mut self.endpoint, 4096)),
            Step::Secret if self.auth == Auth::File => Some((&mut self.key_file, 4096)),
            Step::Model => Some((&mut self.model, 256)),
            Step::Catalog => Some((&mut self.search, 256)),
            _ => None,
        }
    }

    pub(super) fn paste(&mut self, text: &str, busy: bool) {
        if busy && self.step != Step::Catalog {
            return;
        }
        if self.step == Step::Secret && self.auth == Auth::Managed {
            let text = text.trim_end_matches(['\r', '\n']);
            if self.secret.len() + text.len() <= 8192
                && !text.chars().any(|ch| ch.is_whitespace() || ch.is_control())
            {
                self.secret.push_str(text);
                self.error = None;
            } else {
                self.error = Some("Paste one API key without whitespace.".into());
            }
        } else if !text.chars().any(char::is_control)
            && let Some((editor, limit)) = self.editor()
            && editor.insert(text, limit)
        {
            self.changed_field();
        }
    }

    fn changed_field(&mut self) {
        self.error = None;
        if self.step == Step::Catalog {
            self.catalog.reset_selection();
        }
        if self.step == Step::Endpoint {
            self.secret.zeroize();
            self.key_file.clear();
            self.catalog = Catalog::default();
            self.auth = if self.can_keep() {
                Auth::Keep
            } else {
                Auth::Managed
            };
        }
    }

    fn confirm(&mut self) -> Option<Request> {
        self.error = None;
        match self.step {
            Step::Name => {
                let profile = self.name.text().trim();
                if profile.is_empty() {
                    self.error = Some("Enter a profile name.".into());
                    return None;
                }
                return Some(Request::OpenModelConnection(
                    ModelProfileConnectionGetParams {
                        target: ModelProfileConnectionTarget::New {
                            profile: profile.into(),
                        },
                    },
                ));
            }
            Step::Protocol => self.step = Step::Endpoint,
            Step::Endpoint => {
                if let Some(url) = self.endpoint()
                    && let Err(error) = provider_api_root(url)
                {
                    self.error = Some(error.into());
                    return None;
                }
                if !self.auth_options().contains(&self.auth) {
                    self.auth = Auth::Managed;
                }
                self.step = Step::Authentication;
            }
            Step::Authentication => {
                self.step = if matches!(self.auth, Auth::Managed | Auth::File) {
                    Step::Secret
                } else {
                    Step::Model
                };
            }
            Step::Secret => {
                if self.auth == Auth::Managed && validate_api_key(&self.secret).is_err() {
                    self.error = Some("Paste an API key to continue.".into());
                } else if self.auth == Auth::File && self.key_file.text().trim().is_empty() {
                    self.error = Some("Enter the API key file path.".into());
                } else {
                    self.step = Step::Model;
                }
            }
            Step::Model => return self.save(),
            Step::Catalog => {
                if let Some(id) = self.catalog.selected(self.search.text()) {
                    self.model.clear();
                    let _ = self.model.insert(id, 256);
                    self.step = Step::Model;
                }
            }
            Step::Effort => {
                self.effort = self
                    .effort_index
                    .checked_sub(1)
                    .and_then(|idx| {
                        MODEL_ROUTES[self.route]
                            .provider
                            .reasoning_efforts()
                            .get(idx)
                    })
                    .map(|effort| effort.as_str().to_owned());
                self.step = Step::Model;
            }
        }
        None
    }

    fn save(&mut self) -> Option<Request> {
        if self.model.text().trim().is_empty() {
            self.error = Some("Enter a model ID or choose one from the catalog.".into());
            return None;
        }
        Some(Request::SaveModelConnection(
            ModelProfileConnectionSaveParams {
                target: self.target.clone()?,
                revision: self.original.as_ref()?.revision.clone(),
                model: self.model.text().trim().into(),
                reasoning_effort: self.effort.clone(),
                connection: self.connection(),
            },
        ))
    }

    pub(super) fn key(&mut self, key: KeyEvent, busy: bool) -> Option<Request> {
        if busy && self.step != Step::Catalog {
            return None;
        }
        let pressed = key.kind == KeyEventKind::Press;
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match (self.step, key.code) {
            (_, KeyCode::Enter) if pressed && !busy => return self.confirm(),
            (Step::Catalog, KeyCode::Tab) if pressed && !busy => return self.confirm(),
            (Step::Model, KeyCode::Char('s')) if ctrl && pressed => return self.confirm(),
            (Step::Model, KeyCode::F(5)) if pressed => {
                self.search.clear();
                self.step = Step::Catalog;
                return self.browse(None);
            }
            (Step::Catalog, KeyCode::F(5)) if pressed && !busy => return self.browse(None),
            (Step::Catalog, KeyCode::F(6)) if pressed && !busy => {
                return self.browse(Some(self.catalog.cursor()?.to_owned()));
            }
            (Step::Model, KeyCode::F(3)) if pressed => {
                self.effort_index = MODEL_ROUTES[self.route]
                    .provider
                    .reasoning_efforts()
                    .iter()
                    .position(|effort| Some(effort.as_str()) == self.effort.as_deref())
                    .map_or(0, |idx| idx + 1);
                self.step = Step::Effort;
                self.error = None;
            }
            (Step::Protocol, code) => {
                let old = self.route;
                step(code, &mut self.route, MODEL_ROUTES.len());
                if old != self.route {
                    self.secret.zeroize();
                    self.key_file.clear();
                    self.catalog = Catalog::default();
                    if !MODEL_ROUTES[self.route]
                        .provider
                        .reasoning_efforts()
                        .iter()
                        .any(|effort| Some(effort.as_str()) == self.effort.as_deref())
                    {
                        self.effort = None;
                    }
                    self.auth = if self.can_keep() {
                        Auth::Keep
                    } else {
                        Auth::Managed
                    };
                }
            }
            (Step::Authentication, code) => {
                let options = self.auth_options();
                let mut i = options
                    .iter()
                    .position(|option| *option == self.auth)
                    .unwrap_or(0);
                step(code, &mut i, options.len());
                if self.auth != options[i] {
                    self.secret.zeroize();
                    self.auth = options[i];
                }
            }
            (Step::Effort, code) => step(
                code,
                &mut self.effort_index,
                MODEL_ROUTES[self.route].provider.reasoning_efforts().len() + 1,
            ),
            (
                Step::Catalog,
                code @ (KeyCode::Up | KeyCode::Down | KeyCode::PageUp | KeyCode::PageDown),
            ) => {
                self.catalog.navigate(code, self.search.text());
            }
            (Step::Secret, KeyCode::Backspace) if self.auth == Auth::Managed => {
                if let Some((offset, tail)) = self.secret.char_indices().next_back() {
                    self.secret
                        .replace_range(offset.., &"*".repeat(tail.len_utf8()));
                    self.secret.truncate(offset);
                }
                self.error = None;
            }
            (_, KeyCode::Char(ch)) if !ctrl && !key.modifiers.contains(KeyModifiers::ALT) => {
                self.paste(ch.encode_utf8(&mut [0; 4]), busy);
            }
            (_, code) => {
                if let Some((editor, _)) = self.editor()
                    && editor.key(code)
                {
                    self.changed_field();
                }
            }
        }
        None
    }

    pub(super) fn title(&self) -> &'static str {
        match self.step {
            Step::Name => "Add model profile",
            Step::Protocol => "Connection · API protocol",
            Step::Endpoint => "Connection · Base URL",
            Step::Authentication => "Connection · Authentication",
            Step::Secret if self.auth == Auth::File => "Connection · API key file",
            Step::Secret => "Connection · API key",
            Step::Model => "Connection · Model and save",
            Step::Catalog => "Browse connection catalog",
            Step::Effort => "Connection · Thinking effort",
        }
    }

    pub(super) fn preferred_height(&self) -> u16 {
        let rows = match self.step {
            Step::Catalog => (6 + self.catalog.visible_rows(self.search.text())).clamp(8, 13),
            Step::Effort => 13,
            Step::Authentication | Step::Protocol => 10,
            _ => 7,
        };
        rows + u16::from(self.error.is_some()) * 2
    }

    pub(super) fn draw(&self, frame: &mut Frame<'_>, body: Rect, palette: &Palette) {
        let help = match self.step {
            Step::Model => "F5 Browse   F3 Thinking   Enter Save",
            Step::Catalog if self.catalog.cursor().is_some() => "Enter Pick   F5 Reload   F6 More",
            Step::Catalog => "Enter Pick   F5 Reload",
            Step::Protocol | Step::Authentication | Step::Effort => "↑ ↓ Select   Enter Continue",
            _ => "Enter Continue",
        };
        let mut foot = vec![Line::styled(help, palette.quiet)];
        if let Some(error) = &self.error {
            foot.push(Line::styled(clean(error), palette.status(Color::Red)));
        }
        let footer = Paragraph::new(foot).wrap(Wrap { trim: true });
        let footer_height =
            (footer.line_count(body.width) as u16).min(body.height.saturating_sub(1));
        let [main, hint] =
            Layout::vertical([Constraint::Min(1), Constraint::Length(footer_height)]).areas(body);
        frame.render_widget(footer, hint);
        match self.step {
            Step::Name => self.draw_entry(
                frame,
                main,
                &self.name,
                "Profile name",
                "Letters, digits, hyphens and underscores.",
                palette,
            ),
            Step::Protocol => render_list(
                frame,
                main,
                MODEL_ROUTES
                    .iter()
                    .map(|route| route.label.into())
                    .collect(),
                self.route,
                palette.choice,
            ),
            Step::Endpoint => self.draw_entry(
                frame,
                main,
                &self.endpoint,
                "Provider default",
                "Enter the API root, or leave blank for the default.",
                palette,
            ),
            Step::Authentication => {
                let options = self.auth_options();
                let selected = options
                    .iter()
                    .position(|auth| *auth == self.auth)
                    .unwrap_or(0);
                let explanation = if self.can_keep() {
                    "Choose how to authenticate."
                } else {
                    "Choose credentials for this connection."
                };
                let [header, list] =
                    Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(main);
                frame.render_widget(Line::styled(explanation, palette.quiet), header);
                render_list(
                    frame,
                    list,
                    options.iter().map(|auth| auth.label().into()).collect(),
                    selected,
                    palette.choice,
                );
            }
            Step::Secret if self.auth == Auth::File => self.draw_entry(
                frame,
                main,
                &self.key_file,
                "Path to key file",
                "Absolute path or relative to config.toml.",
                palette,
            ),
            Step::Secret => {
                let [row, status] =
                    Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(main);
                let prompt = if self.secret.is_empty() {
                    "› Paste API key".into()
                } else {
                    format!(
                        "› {}",
                        "•".repeat(
                            self.secret
                                .chars()
                                .count()
                                .min(usize::from(row.width.saturating_sub(2)))
                        )
                    )
                };
                frame.render_widget(Paragraph::new(prompt).style(palette.rail), row);
                frame.render_widget(
                    Line::styled("Your key will be stored privately.", palette.quiet),
                    status,
                );
            }
            Step::Model => {
                let effort = self.effort.as_deref().unwrap_or("provider default");
                self.draw_entry(
                    frame,
                    main,
                    &self.model,
                    "Model ID",
                    &format!("Thinking: {effort}"),
                    palette,
                );
            }
            Step::Catalog => {
                let status = self.catalog.status(self.search.text());
                let status_text = Paragraph::new(status)
                    .style(palette.quiet)
                    .wrap(Wrap { trim: true });
                let status_rows = if status.is_empty() || self.error.is_some() {
                    0
                } else {
                    status_text.line_count(main.width).min(2) as u16
                };
                let [input, note, list] = Layout::vertical([
                    Constraint::Length(1),
                    Constraint::Length(status_rows),
                    Constraint::Min(0),
                ])
                .areas(main);
                draw_field(frame, input, &self.search, "Search model IDs", palette);
                frame.render_widget(status_text, note);
                self.catalog
                    .draw(frame, list, self.search.text(), palette.choice);
            }
            Step::Effort => {
                let labels = std::iter::once("Provider default")
                    .chain(
                        MODEL_ROUTES[self.route]
                            .provider
                            .reasoning_efforts()
                            .iter()
                            .map(|effort| effort.as_str()),
                    )
                    .map(str::to_owned)
                    .collect();
                render_list(frame, main, labels, self.effort_index, palette.choice);
            }
        }
    }

    fn draw_entry(
        &self,
        frame: &mut Frame<'_>,
        area: Rect,
        editor: &Editor,
        placeholder: &str,
        note: &str,
        palette: &Palette,
    ) {
        let [input, help] =
            Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(area);
        draw_field(frame, input, editor, placeholder, palette);
        frame.render_widget(
            Paragraph::new(note)
                .style(palette.quiet)
                .wrap(Wrap { trim: true }),
            help,
        );
    }
}
