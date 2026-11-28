use super::{model_edit::draw_field, setup::MODEL_ROUTES, *};
use fliuno_config::{is_local_provider_root, provider_api_root, validate_api_key};
use fliuno_protocol::{
    ModelCatalogPreview, ModelProfileConnectionTarget, ModelProfileCredentialInput,
    ModelProfileCredentialStatus, ModelSupplierBindingParams, ModelSupplierCatalogParams,
    ModelSupplierEntry, ModelSupplierPreset, ModelSupplierProfile, ModelSupplierSaveParams,
    ModelSupplierSnapshot, ModelSupplierTarget,
};
use zeroize::{Zeroize, Zeroizing};

mod forms;
use forms::{Binding, Supplier};

#[derive(Default)]
pub(super) struct Panel {
    snapshot: Option<ModelSupplierSnapshot>,
    browse: bool,
    language: Language,
    models: bool,
    selected: usize,
    form: Option<Form>,
    saved: bool,
    error: Option<String>,
}

enum Form {
    Supplier(Supplier),
    Binding(Box<Binding>),
}

impl Panel {
    pub fn new(browse: bool, language: Language) -> Self {
        Self {
            browse,
            language,
            ..Self::default()
        }
    }

    pub fn install_catalog(&mut self, preview: ModelCatalogPreview) {
        if let Some(Form::Binding(form)) = &mut self.form {
            form.install_catalog(preview);
        }
    }

    pub fn preferred_height(&self) -> u16 {
        match &self.form {
            Some(Form::Binding(form)) => form.catalog_height().unwrap_or(15),
            _ => 15,
        }
    }

    pub fn title(&self) -> &'static str {
        let language = self.language;
        match &self.form {
            Some(Form::Supplier(_)) => language.text("Supplier connection", "供应商连接"),
            Some(Form::Binding(form)) if form.in_catalog() => {
                language.text("Supplier model catalog", "供应商模型目录")
            }
            Some(Form::Binding(_)) => language.text("Model binding", "模型绑定"),
            None => language.text("Model settings", "模型设置"),
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
        if text.chars().any(char::is_control) {
            return;
        }
        match &mut self.form {
            Some(Form::Supplier(form)) if !busy => form.paste(text),
            Some(Form::Binding(form)) if !busy || form.in_catalog() => form.paste(text),
            _ => (),
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

    pub fn key(&mut self, key: KeyEvent, busy: bool) -> Option<Request> {
        let in_catalog = matches!(&self.form, Some(Form::Binding(form)) if form.in_catalog());
        if key.kind == KeyEventKind::Release || (busy && !in_catalog) {
            return None;
        }
        if !in_catalog && key.code == KeyCode::F(5) && key.kind == KeyEventKind::Press {
            self.form = None;
            return Some(Request::SupplierSettings);
        }
        let snapshot = self.snapshot.as_ref()?;
        if let Some(form) = &mut self.form {
            self.error = None;
            let next = match form {
                Form::Supplier(form) => form.key(key, &snapshot.revision, self.language),
                Form::Binding(form) => form.key(key, snapshot, busy, self.language),
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
                        self.error = Some(
                            self.language
                                .text("Add a supplier connection first.", "请先添加供应商连接。")
                                .into(),
                        );
                        return None;
                    }
                    let entry = snapshot.models.get(self.selected);
                    if !create && entry.is_none() {
                        return None;
                    }
                    Some(Form::Binding(Box::new(Binding::new(
                        entry.filter(|_| !create),
                        snapshot,
                        self.browse,
                    ))))
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
        let language = self.language;
        let review = match &self.form {
            Some(Form::Supplier(form)) => form.reviewing(),
            Some(Form::Binding(form)) => form.reviewing(),
            None => false,
        };
        let mut hints = if self.form.is_some() {
            let primary = if review {
                language.text("Enter Save", "Enter 保存")
            } else {
                language.text("Enter Next", "Enter 下一步")
            };
            let navigation = if review {
                language.text("  ·  ↑↓ Scroll  ·  Esc Back", "  ·  ↑↓ 滚动  ·  Esc 返回")
            } else {
                language.text("  ·  Esc Back", "  ·  Esc 返回")
            };
            vec![
                Line::from(vec![
                    Span::styled(primary, palette.label),
                    Span::styled(navigation, palette.quiet),
                ]),
                Line::styled(
                    language.text("F5 Discard draft and reload", "F5 放弃草稿并重载"),
                    palette.quiet,
                ),
            ]
        } else {
            vec![
                Line::styled(
                    language.text(
                        "Tab Switch  ·  N Add  ·  Enter Edit",
                        "Tab 切换  ·  N 新增  ·  Enter 编辑",
                    ),
                    palette.label,
                ),
                Line::styled(
                    language.text(
                        "↑↓ Select  ·  F5 Reload  ·  Esc Close",
                        "↑↓ 选择  ·  F5 重载  ·  Esc 关闭",
                    ),
                    palette.quiet,
                ),
            ]
        };
        if let Some(Form::Binding(form)) = &self.form
            && let Some(catalog_hints) = form.catalog_hints(language)
        {
            hints = catalog_hints
                .iter()
                .enumerate()
                .map(|(i, text)| {
                    Line::styled(*text, if i == 0 { palette.label } else { palette.quiet })
                })
                .collect();
        }
        if self.saved && self.form.is_none() {
            hints.push(Line::styled(
                language.text(
                    "Saved. Use /model to apply a model.",
                    "已保存。用 /model 启用模型。",
                ),
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
            (Some(Form::Supplier(form)), _) => form.draw(frame, content, palette, language),
            (Some(Form::Binding(form)), Some(snapshot)) => {
                form.draw(frame, content, palette, snapshot, language)
            }
            (_, Some(snapshot)) => self.draw_list(frame, content, palette, snapshot),
            (_, None) => render_text(
                frame,
                content,
                if self.error.is_some() {
                    language.text("F5 to reload settings.", "按 F5 重载设置。")
                } else {
                    language.text("Reading model settings…", "正在读取模型设置…")
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
        let language = self.language;
        let [tabs, list] =
            Layout::vertical([Constraint::Length(2), Constraint::Min(1)]).areas(area);
        let tab_style = |active| if active { palette.label } else { palette.quiet };
        frame.render_widget(
            Line::from(vec![
                Span::styled(
                    if self.models {
                        language.text("Suppliers", "供应商")
                    } else {
                        language.text("[Suppliers]", "[供应商]")
                    },
                    tab_style(!self.models),
                ),
                Span::raw("   "),
                Span::styled(
                    if self.models {
                        language.text("[Models]", "[模型]")
                    } else {
                        language.text("Models", "模型")
                    },
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
            render_text(
                frame,
                list,
                language.text(
                    "No named suppliers. Press N to add one.",
                    "还没有命名供应商。按 N 添加。",
                ),
                0,
            );
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
                        format!(
                            "{}  ·  {models} {}",
                            supplier_label(entry, language),
                            language.text("bound", "个绑定")
                        )
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

fn supplier_label(entry: &ModelSupplierEntry, language: Language) -> String {
    let credential = match entry.credential {
        ModelProfileCredentialStatus::Configured => language.text("key set", "已配置密钥"),
        ModelProfileCredentialStatus::Keyless => language.text("no key", "无密钥"),
        ModelProfileCredentialStatus::Unconfigured => language.text("key missing", "缺少密钥"),
    };
    format!("{}  ·  {credential}", entry.id)
}
