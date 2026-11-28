use super::{model_edit::draw_field, setup::MODEL_ROUTES, *};
use fliuno_config::{is_local_provider_root, provider_api_root, validate_api_key};
use fliuno_protocol::{
    ModelProfileConnectionTarget, ModelProfileCredentialInput, ModelProfileCredentialStatus,
    ModelSupplierBindingParams, ModelSupplierEntry, ModelSupplierPreset, ModelSupplierProfile,
    ModelSupplierSaveParams, ModelSupplierSnapshot, ModelSupplierTarget,
};
use zeroize::{Zeroize, Zeroizing};

mod forms;
use forms::{Binding, Supplier};

#[derive(Default)]
pub(super) struct Panel {
    snapshot: Option<ModelSupplierSnapshot>,
    models: bool,
    selected: usize,
    form: Option<Form>,
    saved: bool,
    error: Option<String>,
}

enum Form {
    Supplier(Supplier),
    Binding(Binding),
}

impl Panel {
    pub fn title(&self) -> &'static str {
        match &self.form {
            Some(Form::Supplier(_)) => "Supplier connection",
            Some(Form::Binding(_)) => "Model binding",
            None => "Model settings",
        }
    }

    pub fn install(&mut self, snapshot: ModelSupplierSnapshot) {
        if let Some(previous) = &self.snapshot {
            let same_entry = if self.models {
                previous.models.get(self.selected).and_then(|selected| {
                    snapshot
                        .models
                        .iter()
                        .position(|entry| entry.profile == selected.profile)
                })
            } else {
                previous.suppliers.get(self.selected).and_then(|selected| {
                    snapshot
                        .suppliers
                        .iter()
                        .position(|entry| entry.id == selected.id)
                })
            };
            if let Some(index) = same_entry {
                self.selected = index;
            }
        }
        self.snapshot = Some(snapshot);
        self.selected = self.selected.min(self.count().saturating_sub(1));
        self.error = None;
    }

    pub fn saved(&mut self) {
        self.form = None;
        self.snapshot = None;
        self.saved = true;
        self.error = None;
    }

    pub fn failed(&mut self, error: String) {
        self.error = Some(error);
    }

    pub fn back(&mut self) -> bool {
        self.error = None;
        let close = match &mut self.form {
            Some(Form::Supplier(form)) => form.back(),
            Some(Form::Binding(form)) => form.back(),
            None => return true,
        };
        if close {
            self.form = None;
        }
        false
    }

    pub fn paste(&mut self, text: &str, busy: bool) {
        if busy || text.chars().any(char::is_control) {
            return;
        }
        match &mut self.form {
            Some(Form::Supplier(form)) => form.paste(text),
            Some(Form::Binding(form)) => form.paste(text),
            None => (),
        }
    }

    fn count(&self) -> usize {
        self.snapshot.as_ref().map_or(0, |snapshot| {
            if self.models {
                snapshot.models.len()
            } else {
                snapshot.suppliers.len()
            }
        })
    }

    pub fn key(&mut self, key: KeyEvent) -> Option<Request> {
        if key.kind == KeyEventKind::Release {
            return None;
        }
        if key.code == KeyCode::F(5) && key.kind == KeyEventKind::Press {
            self.form = None;
            return Some(Request::SupplierSettings);
        }
        let snapshot = self.snapshot.as_ref()?;
        if let Some(form) = &mut self.form {
            self.error = None;
            let next = match form {
                Form::Supplier(form) => form.key(key, &snapshot.revision),
                Form::Binding(form) => form.key(key, snapshot),
            };
            return match next {
                Ok(request) => request,
                Err(error) => {
                    self.error = Some(error);
                    None
                }
            };
        }
        let len = self.count();
        step(key.code, &mut self.selected, len);
        if key.kind != KeyEventKind::Press {
            return None;
        }
        match key.code {
            KeyCode::Tab | KeyCode::BackTab => {
                self.models = !self.models;
                self.selected = 0;
                self.error = None;
            }
            KeyCode::Char('n') | KeyCode::Enter => {
                let create = key.code == KeyCode::Char('n');
                self.error = None;
                self.form = if self.models {
                    if snapshot.suppliers.is_empty() {
                        self.error = Some("Add a supplier connection first.".into());
                        return None;
                    }
                    let entry = snapshot.models.get(self.selected);
                    if !create && entry.is_none() {
                        return None;
                    }
                    Some(Form::Binding(Binding::new(
                        entry.filter(|_| !create),
                        snapshot,
                    )))
                } else {
                    let entry = snapshot.suppliers.get(self.selected);
                    if !create && entry.is_none() {
                        return None;
                    }
                    Some(Form::Supplier(Supplier::new(entry.filter(|_| !create))))
                };
            }
            _ => (),
        }
        None
    }

    pub fn draw(&self, frame: &mut Frame<'_>, body: Rect, palette: &Palette) {
        let review = match &self.form {
            Some(Form::Supplier(form)) => form.reviewing(),
            Some(Form::Binding(form)) => form.reviewing(),
            None => false,
        };
        let mut hints = if self.form.is_some() {
            let primary = if review { "Enter Save" } else { "Enter Next" };
            let navigation = if review {
                "  ·  ↑↓ Scroll  ·  Esc Back"
            } else {
                "  ·  Esc Back"
            };
            vec![
                Line::from(vec![
                    Span::styled(primary, palette.label),
                    Span::styled(navigation, palette.quiet),
                ]),
                Line::styled("F5 Discard draft and reload", palette.quiet),
            ]
        } else {
            vec![
                Line::styled("Tab Switch  ·  N Add  ·  Enter Edit", palette.label),
                Line::styled("↑↓ Select  ·  F5 Reload  ·  Esc Close", palette.quiet),
            ]
        };
        if self.saved && self.form.is_none() {
            hints.push(Line::styled(
                "Saved. Use /model to apply a model.",
                palette.label,
            ));
        }
        if let Some(error) = &self.error {
            hints.push(Line::styled(clean(error), palette.status(Color::Red)));
        }
        let footer = Paragraph::new(hints).wrap(Wrap { trim: true });
        let footer_height =
            (footer.line_count(body.width.max(1)) as u16).min(body.height.saturating_sub(1));
        let [content, tail] =
            Layout::vertical([Constraint::Min(1), Constraint::Length(footer_height)]).areas(body);
        frame.render_widget(footer, tail);
        match (&self.form, &self.snapshot) {
            (Some(Form::Supplier(form)), _) => form.draw(frame, content, palette),
            (Some(Form::Binding(form)), Some(snapshot)) => {
                form.draw(frame, content, palette, snapshot)
            }
            (_, Some(snapshot)) => self.draw_list(frame, content, palette, snapshot),
            (_, None) => render_text(
                frame,
                content,
                if self.error.is_some() {
                    "F5 to reload settings."
                } else {
                    "Reading model settings…"
                },
                0,
            ),
        }
    }

    fn draw_list(
        &self,
        frame: &mut Frame<'_>,
        area: Rect,
        palette: &Palette,
        snapshot: &ModelSupplierSnapshot,
    ) {
        let [tabs, list] =
            Layout::vertical([Constraint::Length(2), Constraint::Min(1)]).areas(area);
        let tab_style = |active| if active { palette.label } else { palette.quiet };
        frame.render_widget(
            Line::from(vec![
                Span::styled(
                    if self.models {
                        "Suppliers"
                    } else {
                        "[Suppliers]"
                    },
                    tab_style(!self.models),
                ),
                Span::raw("   "),
                Span::styled(
                    if self.models { "[Models]" } else { "Models" },
                    tab_style(self.models),
                ),
            ]),
            tabs,
        );
        if self.models {
            render_list(
                frame,
                list,
                snapshot
                    .models
                    .iter()
                    .map(|entry| {
                        let name = entry.profile.as_deref().unwrap_or("[model]");
                        let supplier = entry.supplier.as_deref().unwrap_or("inline");
                        let model = entry.model.as_deref().unwrap_or("unconfigured");
                        format!("{name}  ·  {supplier}/{model}")
                    })
                    .collect(),
                self.selected,
                palette.choice,
            );
        } else if snapshot.suppliers.is_empty() {
            render_text(frame, list, "No named suppliers. Press N to add one.", 0);
        } else {
            let [choices, details] =
                Layout::vertical([Constraint::Min(1), Constraint::Length(3)]).areas(list);
            render_list(
                frame,
                choices,
                snapshot
                    .suppliers
                    .iter()
                    .map(|entry| {
                        let models = snapshot
                            .models
                            .iter()
                            .filter(|model| model.supplier.as_deref() == Some(entry.id.as_str()))
                            .count();
                        format!("{}  ·  {} bound", supplier_label(entry), models)
                    })
                    .collect(),
                self.selected,
                palette.choice,
            );
            if let Some(entry) = snapshot.suppliers.get(self.selected) {
                frame.render_widget(
                    Paragraph::new(vec![
                        Line::styled("Base URL", palette.quiet),
                        Line::raw(clean(&entry.base_url)),
                    ])
                    .wrap(Wrap { trim: true }),
                    details,
                );
            }
        }
    }
}

fn supplier_label(entry: &ModelSupplierEntry) -> String {
    let credential = match entry.credential {
        ModelProfileCredentialStatus::Configured => "key set",
        ModelProfileCredentialStatus::Keyless => "no key",
        ModelProfileCredentialStatus::Unconfigured => "key missing",
    };
    format!("{}  ·  {credential}", entry.id)
}
