use super::*;
use std::cell::Cell;

const PRESETS: [(Option<ModelSupplierPreset>, &str); 4] = [
    (Some(ModelSupplierPreset::OpenAi), "OpenAI"),
    (Some(ModelSupplierPreset::Anthropic), "Anthropic"),
    (Some(ModelSupplierPreset::Gemini), "Google Gemini"),
    (Some(ModelSupplierPreset::Custom), "Custom Base URL"),
];

fn preset_root(index: usize) -> Option<&'static str> {
    use fliuno_config::SupplierPreset;
    match PRESETS[index].0 {
        Some(ModelSupplierPreset::OpenAi) => SupplierPreset::OpenAi.default_root(),
        Some(ModelSupplierPreset::Anthropic) => SupplierPreset::Anthropic.default_root(),
        Some(ModelSupplierPreset::Gemini) => SupplierPreset::Gemini.default_root(),
        _ => None,
    }
}

fn text_input(input: &mut Editor, event: KeyEvent, limit: usize) {
    if let KeyCode::Char(ch) = event.code {
        if !ch.is_control()
            && !event
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
        {
            let _ = input.insert(ch.encode_utf8(&mut [0; 4]), limit);
        }
    } else {
        input.key(event.code);
    }
}

fn editor(initial: &str, limit: usize) -> Editor {
    let mut field = Editor::default();
    let _ = field.insert(initial, limit);
    field
}

fn input(
    frame: &mut Frame<'_>,
    area: Rect,
    palette: &Palette,
    label: &str,
    value: &Editor,
    placeholder: &str,
) {
    let [heading, field, _] = Layout::vertical([
        Constraint::Length(2),
        Constraint::Length(1),
        Constraint::Min(0),
    ])
    .areas(area);
    frame.render_widget(Line::styled(label, palette.label), heading);
    draw_field(frame, field, value, placeholder, palette);
}

fn choices(
    frame: &mut Frame<'_>,
    area: Rect,
    palette: &Palette,
    label: &str,
    entries: Vec<String>,
    index: usize,
) {
    let [heading, list] = Layout::vertical([Constraint::Length(2), Constraint::Min(1)]).areas(area);
    frame.render_widget(Line::styled(label, palette.label), heading);
    render_list(frame, list, entries, index, palette.choice);
}

fn identifier(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 48
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
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
            Self::Keep => "Keep existing credential settings",
            Self::Managed => "Enter API key",
            Self::File => "Read from API key file",
            Self::None => "No API key (loopback only)",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SupplierStep {
    Name,
    Preset,
    BaseUrl,
    Auth,
    Credential,
    Review,
}

pub(super) struct Supplier {
    original: Option<ModelSupplierEntry>,
    step: SupplierStep,
    id: Editor,
    base: Editor,
    preset: usize,
    auth: Auth,
    key_file: Editor,
    key: Zeroizing<String>,
    review: ReviewScroll,
}

impl Supplier {
    pub fn new(original: Option<&ModelSupplierEntry>) -> Self {
        let preset = original.map_or(0, |entry| {
            PRESETS
                .iter()
                .position(|(preset, _)| *preset == entry.preset)
                .unwrap_or(3)
        });
        let endpoint = original.map_or("", |entry| entry.base_url.as_str());
        Self {
            original: original.cloned(),
            step: if original.is_some() {
                SupplierStep::Preset
            } else {
                SupplierStep::Name
            },
            id: editor(original.map_or("", |entry| &entry.id), 48),
            base: editor(
                if preset_root(preset) == Some(endpoint) {
                    ""
                } else {
                    endpoint
                },
                4096,
            ),
            preset,
            auth: if original.is_some() {
                Auth::Keep
            } else {
                Auth::Managed
            },
            key_file: Editor::default(),
            key: Zeroizing::new(String::new()),
            review: ReviewScroll::default(),
        }
    }

    fn root(&self) -> Option<&str> {
        let value = self.base.text().trim();
        if value.is_empty() {
            preset_root(self.preset)
        } else {
            Some(value)
        }
    }

    fn auths(&self) -> Vec<Auth> {
        let mut options = Vec::with_capacity(4);
        if self.original.as_ref().is_some_and(|entry| {
            entry.credential != ModelProfileCredentialStatus::Unconfigured
                && self
                    .root()
                    .and_then(|root| provider_api_root(root).ok())
                    .is_some_and(|root| root.as_str() == entry.base_url)
        }) {
            options.push(Auth::Keep);
        }
        options.extend([Auth::Managed, Auth::File]);
        if self
            .root()
            .and_then(|root| provider_api_root(root).ok())
            .is_some_and(|url| is_local_provider_root(&url))
        {
            options.push(Auth::None);
        }
        options
    }

    pub fn reviewing(&self) -> bool {
        self.step == SupplierStep::Review
    }

    pub fn back(&mut self) -> bool {
        self.review.reset();
        self.step = match self.step {
            SupplierStep::Name => return true,
            SupplierStep::Preset if self.original.is_some() => return true,
            SupplierStep::Preset => SupplierStep::Name,
            SupplierStep::BaseUrl => SupplierStep::Preset,
            SupplierStep::Auth => SupplierStep::BaseUrl,
            SupplierStep::Credential => {
                self.key.zeroize();
                SupplierStep::Auth
            }
            SupplierStep::Review if matches!(self.auth, Auth::File | Auth::Managed) => {
                SupplierStep::Credential
            }
            SupplierStep::Review => SupplierStep::Auth,
        };
        false
    }

    pub fn paste(&mut self, text: &str) {
        match self.step {
            SupplierStep::Name => {
                let _ = self.id.insert(text, 48);
            }
            SupplierStep::BaseUrl => {
                let _ = self.base.insert(text, 4096);
            }
            SupplierStep::Credential if self.auth == Auth::File => {
                let _ = self.key_file.insert(text, 4096);
            }
            SupplierStep::Credential
                if self.auth == Auth::Managed && self.key.len() + text.len() <= 8192 =>
            {
                self.key.push_str(text)
            }
            _ => (),
        }
    }

    pub fn key(&mut self, key: KeyEvent, revision: &str) -> Result<Option<Request>, String> {
        if key.code == KeyCode::Enter && key.kind == KeyEventKind::Press {
            self.step = match self.step {
                SupplierStep::Name => {
                    if !identifier(self.id.text()) {
                        return Err("Use 1–48 letters, digits, hyphens or underscores.".into());
                    }
                    SupplierStep::Preset
                }
                SupplierStep::Preset => SupplierStep::BaseUrl,
                SupplierStep::BaseUrl => {
                    provider_api_root(self.root().ok_or("Enter a Base URL.")?)?;
                    let auths = self.auths();
                    if !auths.contains(&self.auth) {
                        self.auth = auths[0];
                    }
                    SupplierStep::Auth
                }
                SupplierStep::Auth if matches!(self.auth, Auth::Managed | Auth::File) => {
                    SupplierStep::Credential
                }
                SupplierStep::Auth => SupplierStep::Review,
                SupplierStep::Credential => {
                    if self.auth == Auth::Managed {
                        validate_api_key(&self.key)?;
                    }
                    if self.auth == Auth::File && self.key_file.text().trim().is_empty() {
                        return Err("Enter the API key file path.".into());
                    }
                    SupplierStep::Review
                }
                SupplierStep::Review => {
                    let supplier = self.id.text().to_owned();
                    let target = if self.original.is_some() {
                        ModelSupplierTarget::Existing { supplier }
                    } else {
                        ModelSupplierTarget::New { supplier }
                    };
                    let endpoint = self.base.text().trim();
                    let credential = match self.auth {
                        Auth::Keep => ModelProfileCredentialInput::Keep,
                        Auth::Managed => ModelProfileCredentialInput::Managed {
                            key: self.key.to_string(),
                        },
                        Auth::File => ModelProfileCredentialInput::File {
                            path: self.key_file.text().trim().into(),
                        },
                        Auth::None => ModelProfileCredentialInput::None,
                    };
                    return Ok(Some(Request::SaveSupplier(ModelSupplierSaveParams {
                        target,
                        revision: revision.into(),
                        preset: PRESETS[self.preset].0,
                        base_url: (!endpoint.is_empty()).then(|| endpoint.into()),
                        credential,
                    })));
                }
            };
            return Ok(None);
        }
        match self.step {
            SupplierStep::Name => text_input(&mut self.id, key, 48),
            SupplierStep::Preset => step(key.code, &mut self.preset, PRESETS.len()),
            SupplierStep::BaseUrl => text_input(&mut self.base, key, 4096),
            SupplierStep::Auth => {
                let options = self.auths();
                let mut index = options
                    .iter()
                    .position(|item| *item == self.auth)
                    .unwrap_or(0);
                step(key.code, &mut index, options.len());
                if self.auth != options[index] {
                    self.key.zeroize();
                    self.auth = options[index];
                }
            }
            SupplierStep::Credential if self.auth == Auth::File => {
                text_input(&mut self.key_file, key, 4096)
            }
            SupplierStep::Review => self.review.key(key.code),
            SupplierStep::Credential if self.auth == Auth::Managed => match key.code {
                KeyCode::Backspace => {
                    self.key.pop();
                }
                KeyCode::Char(ch)
                    if !ch.is_control()
                        && !key
                            .modifiers
                            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
                        && self.key.len() + ch.len_utf8() <= 8192 =>
                {
                    self.key.push(ch)
                }
                _ => (),
            },
            _ => (),
        }
        Ok(None)
    }

    pub fn draw(&self, frame: &mut Frame<'_>, area: Rect, palette: &Palette) {
        match self.step {
            SupplierStep::Name => {
                input(frame, area, palette, "Supplier ID", &self.id, "e.g. office")
            }
            SupplierStep::Preset => choices(
                frame,
                area,
                palette,
                "Endpoint preset",
                PRESETS.iter().map(|(_, label)| (*label).into()).collect(),
                self.preset,
            ),
            SupplierStep::BaseUrl => input(
                frame,
                area,
                palette,
                "Base URL",
                &self.base,
                preset_root(self.preset).unwrap_or("https://example.com/v1"),
            ),
            SupplierStep::Auth => {
                let auths = self.auths();
                choices(
                    frame,
                    area,
                    palette,
                    "Credentials",
                    auths.iter().map(|a| a.label().into()).collect(),
                    auths.iter().position(|a| *a == self.auth).unwrap_or(0),
                );
            }
            SupplierStep::Credential if self.auth == Auth::Managed => {
                let mask = "•".repeat(self.key.chars().count().min(area.width as usize));
                input(
                    frame,
                    area,
                    palette,
                    "API key · hidden",
                    &editor(&mask, 4096),
                    "Paste or type your key",
                );
            }
            SupplierStep::Credential => input(
                frame,
                area,
                palette,
                "API key file",
                &self.key_file,
                "Path relative to config.toml",
            ),
            SupplierStep::Review => self.review.draw(
                frame,
                area,
                palette,
                &[
                    ("Supplier", self.id.text()),
                    ("Base URL", self.root().unwrap_or_default()),
                    ("Credentials", self.auth.label()),
                ],
            ),
        }
    }
}

#[derive(Default)]
struct ReviewScroll {
    offset: Cell<u16>,
}

impl ReviewScroll {
    fn reset(&self) {
        self.offset.set(0);
    }

    fn key(&self, key: KeyCode) {
        self.offset.set(match key {
            KeyCode::Home => 0,
            KeyCode::End => u16::MAX,
            _ => self.offset.get().saturating_add_signed(scroll_delta(key)),
        });
    }

    fn draw(&self, frame: &mut Frame<'_>, area: Rect, palette: &Palette, values: &[(&str, &str)]) {
        let [title, body] =
            Layout::vertical([Constraint::Length(2), Constraint::Min(0)]).areas(area);
        frame.render_widget(Line::styled("Review and save", palette.label), title);
        let lines = values
            .iter()
            .map(|(label, value)| {
                Line::from(vec![
                    Span::styled(format!("{label}: "), palette.quiet),
                    Span::raw(clean(value)),
                ])
            })
            .collect::<Vec<_>>();
        let details = Paragraph::new(lines).wrap(Wrap { trim: true });
        let extent = details
            .line_count(body.width.max(1))
            .saturating_sub(usize::from(body.height));
        let offset = self.offset.get().min(extent.min(u16::MAX as usize) as u16);
        self.offset.set(offset);
        frame.render_widget(details.scroll((offset, 0)), body);
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum BindingStep {
    Name,
    Supplier,
    Protocol,
    Model,
    Effort,
    Review,
}

pub(super) struct Binding {
    original: Option<ModelSupplierProfile>,
    step: BindingStep,
    id: Editor,
    model: Editor,
    supplier: usize,
    route: usize,
    effort: usize,
    review: ReviewScroll,
}

impl Binding {
    pub fn new(original: Option<&ModelSupplierProfile>, snapshot: &ModelSupplierSnapshot) -> Self {
        let supplier = original
            .and_then(|entry| entry.supplier.as_deref())
            .and_then(|id| snapshot.suppliers.iter().position(|entry| entry.id == id))
            .unwrap_or(0);
        let route = original
            .and_then(|entry| entry.protocol.as_deref())
            .and_then(|protocol| {
                MODEL_ROUTES
                    .iter()
                    .position(|route| route.protocol.as_str() == protocol)
            })
            .unwrap_or_else(|| Self::default_route(&snapshot.suppliers[supplier]));
        let effort = original
            .and_then(|entry| entry.reasoning_effort.as_deref())
            .and_then(|effort| {
                MODEL_ROUTES[route]
                    .provider
                    .reasoning_efforts()
                    .iter()
                    .position(|option| option.as_str() == effort)
            })
            .map_or(0, |index| index + 1);
        Self {
            original: original.cloned(),
            step: if original.is_some() {
                BindingStep::Supplier
            } else {
                BindingStep::Name
            },
            id: Editor::default(),
            model: editor(
                original
                    .and_then(|entry| entry.model.as_deref())
                    .unwrap_or_default(),
                256,
            ),
            supplier,
            route,
            effort,
            review: ReviewScroll::default(),
        }
    }

    fn default_route(supplier: &ModelSupplierEntry) -> usize {
        use fliuno_config::ModelProtocol;
        let protocol = if supplier.credential == ModelProfileCredentialStatus::Keyless {
            ModelProtocol::ChatCompletions
        } else {
            match supplier.preset {
                Some(ModelSupplierPreset::Anthropic) => ModelProtocol::Messages,
                Some(ModelSupplierPreset::Gemini) => ModelProtocol::Interactions,
                Some(ModelSupplierPreset::OpenAi) => ModelProtocol::Responses,
                _ => ModelProtocol::ChatCompletions,
            }
        };
        MODEL_ROUTES
            .iter()
            .position(|route| route.protocol == protocol)
            .expect("supported binding route")
    }

    fn routes(supplier: &ModelSupplierEntry) -> impl Iterator<Item = usize> + use<> {
        let keyless = supplier.credential == ModelProfileCredentialStatus::Keyless;
        (0..MODEL_ROUTES.len()).filter(move |index| {
            !keyless
                || MODEL_ROUTES[*index].protocol == fliuno_config::ModelProtocol::ChatCompletions
        })
    }

    fn efforts(&self) -> impl Iterator<Item = (&'static str, Option<&'static str>)> {
        std::iter::once(("Adapter default", None)).chain(
            MODEL_ROUTES[self.route]
                .provider
                .reasoning_efforts()
                .iter()
                .map(|value| (value.as_str(), Some(value.as_str()))),
        )
    }

    pub fn reviewing(&self) -> bool {
        self.step == BindingStep::Review
    }

    pub fn back(&mut self) -> bool {
        self.review.reset();
        self.step = match self.step {
            BindingStep::Name => return true,
            BindingStep::Supplier if self.original.is_some() => return true,
            BindingStep::Supplier => BindingStep::Name,
            BindingStep::Protocol => BindingStep::Supplier,
            BindingStep::Model => BindingStep::Protocol,
            BindingStep::Effort => BindingStep::Model,
            BindingStep::Review => BindingStep::Effort,
        };
        false
    }

    pub fn paste(&mut self, text: &str) {
        match self.step {
            BindingStep::Name => {
                let _ = self.id.insert(text, 48);
            }
            BindingStep::Model => {
                let _ = self.model.insert(text, 256);
            }
            _ => (),
        }
    }

    pub fn key(
        &mut self,
        event: KeyEvent,
        snapshot: &ModelSupplierSnapshot,
    ) -> Result<Option<Request>, String> {
        let supplier = &snapshot.suppliers[self.supplier];
        if event.code == KeyCode::Enter && event.kind == KeyEventKind::Press {
            self.step = match self.step {
                BindingStep::Name => {
                    if !identifier(self.id.text()) {
                        return Err("Use 1–48 letters, digits, hyphens or underscores.".into());
                    }
                    if snapshot
                        .models
                        .iter()
                        .any(|entry| entry.profile.as_deref() == Some(self.id.text()))
                    {
                        return Err("A model profile already uses this name.".into());
                    }
                    BindingStep::Supplier
                }
                BindingStep::Supplier => {
                    if supplier.credential == ModelProfileCredentialStatus::Unconfigured {
                        return Err("Configure the supplier's credential first.".into());
                    }
                    if !Self::routes(supplier).any(|index| index == self.route) {
                        self.route = Self::default_route(supplier);
                        self.effort = 0;
                    }
                    BindingStep::Protocol
                }
                BindingStep::Protocol => BindingStep::Model,
                BindingStep::Model => {
                    if self.model.text().trim().is_empty() {
                        return Err("Enter the model ID accepted by this endpoint.".into());
                    }
                    BindingStep::Effort
                }
                BindingStep::Effort => BindingStep::Review,
                BindingStep::Review => {
                    let target = match &self.original {
                        None => ModelProfileConnectionTarget::New {
                            profile: self.id.text().into(),
                        },
                        Some(entry) => match &entry.profile {
                            Some(profile) => ModelProfileConnectionTarget::Existing {
                                profile: profile.clone(),
                            },
                            None => ModelProfileConnectionTarget::Base,
                        },
                    };
                    return Ok(Some(Request::BindSupplier(ModelSupplierBindingParams {
                        target,
                        revision: snapshot.revision.clone(),
                        supplier: supplier.id.clone(),
                        protocol: MODEL_ROUTES[self.route].protocol.as_str().into(),
                        model: self.model.text().trim().into(),
                        reasoning_effort: self
                            .efforts()
                            .nth(self.effort)
                            .and_then(|(_, value)| value)
                            .map(str::to_owned),
                    })));
                }
            };
            return Ok(None);
        }
        match self.step {
            BindingStep::Name => text_input(&mut self.id, event, 48),
            BindingStep::Supplier => {
                let old = self.supplier;
                step(event.code, &mut self.supplier, snapshot.suppliers.len());
                if old != self.supplier {
                    self.route = Self::default_route(&snapshot.suppliers[self.supplier]);
                    self.effort = 0;
                }
            }
            BindingStep::Protocol => {
                let routes = Self::routes(supplier).collect::<Vec<_>>();
                let mut index = routes.iter().position(|&i| i == self.route).unwrap_or(0);
                step(event.code, &mut index, routes.len());
                if self.route != routes[index] {
                    self.route = routes[index];
                    self.effort = 0;
                }
            }
            BindingStep::Model => text_input(&mut self.model, event, 256),
            BindingStep::Effort => {
                let count = self.efforts().count();
                step(event.code, &mut self.effort, count);
            }
            BindingStep::Review => self.review.key(event.code),
        }
        Ok(None)
    }

    pub fn draw(
        &self,
        frame: &mut Frame<'_>,
        area: Rect,
        palette: &Palette,
        snapshot: &ModelSupplierSnapshot,
    ) {
        let supplier = &snapshot.suppliers[self.supplier];
        match self.step {
            BindingStep::Name => input(
                frame,
                area,
                palette,
                "Model profile ID",
                &self.id,
                "e.g. daily",
            ),
            BindingStep::Supplier => choices(
                frame,
                area,
                palette,
                "Supplier connection",
                snapshot.suppliers.iter().map(supplier_label).collect(),
                self.supplier,
            ),
            BindingStep::Protocol => {
                let routes = Self::routes(supplier).collect::<Vec<_>>();
                choices(
                    frame,
                    area,
                    palette,
                    "Conversation protocol",
                    routes
                        .iter()
                        .map(|&i| MODEL_ROUTES[i].protocol.as_str().into())
                        .collect(),
                    routes.iter().position(|&i| i == self.route).unwrap_or(0),
                );
            }
            BindingStep::Model => input(
                frame,
                area,
                palette,
                "Model ID",
                &self.model,
                "ID accepted by the endpoint",
            ),
            BindingStep::Effort => choices(
                frame,
                area,
                palette,
                "Reasoning effort",
                self.efforts().map(|(label, _)| label.into()).collect(),
                self.effort,
            ),
            BindingStep::Review => self.review.draw(
                frame,
                area,
                palette,
                &[
                    (
                        "Profile",
                        self.original.as_ref().map_or(self.id.text(), |entry| {
                            entry.profile.as_deref().unwrap_or("[model]")
                        }),
                    ),
                    ("Supplier", &supplier.id),
                    ("Base URL", &supplier.base_url),
                    ("Protocol", MODEL_ROUTES[self.route].protocol.as_str()),
                    ("Model", self.model.text().trim()),
                    (
                        "Reasoning",
                        self.efforts()
                            .nth(self.effort)
                            .map_or("Adapter default", |(label, _)| label),
                    ),
                ],
            ),
        }
    }
}
