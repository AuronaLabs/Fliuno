use super::*;
use fliuno_protocol::{
    ArtifactCatalogScope, ArtifactCatalogSnapshotPage, ArtifactCatalogSnapshotParams,
};

pub(super) struct Catalog {
    workspace: String,
    filter: Filter,
    trail: Vec<u64>,
    page: Option<ArtifactCatalogSnapshotPage>,
    index: usize,
    preview: Option<Box<Preview>>,
    scroll: u16,
    saving: bool,
}

#[derive(Clone, Copy, Default)]
enum Filter {
    #[default]
    All,
    Text,
    Markdown,
    Json,
    Pdf,
}

impl Filter {
    fn next(self) -> Self {
        match self {
            Self::All => Self::Text,
            Self::Text => Self::Markdown,
            Self::Markdown => Self::Json,
            Self::Json => Self::Pdf,
            Self::Pdf => Self::All,
        }
    }
    fn media_type(self) -> Option<&'static str> {
        match self {
            Self::All => None,
            Self::Text => Some("text/plain"),
            Self::Markdown => Some("text/markdown"),
            Self::Json => Some("application/json"),
            Self::Pdf => Some("application/pdf"),
        }
    }
}

impl Catalog {
    pub(super) fn title(&self) -> &'static str {
        if self.saving {
            "Save verified deliverable"
        } else if self.preview.is_some() {
            "Deliverable preview"
        } else {
            "Workspace deliverables"
        }
    }

    pub(super) fn help(&self) -> &'static str {
        if self.saving {
            "Enter Save  ·  Esc Preview"
        } else if let Some(preview) = &self.preview {
            if preview.revision.is_some() {
                "d Save  ·  p Previous version  ·  Esc List"
            } else {
                "d Save  ·  ↑/↓ Scroll  ·  Esc List"
            }
        } else {
            "Enter Open · f Type · PgUp/PgDn · r Refresh · Esc"
        }
    }

    fn request(&self, after: u64, through_sequence: Option<u64>) -> Request {
        Request::Deliverables(ArtifactCatalogSnapshotParams {
            scope: ArtifactCatalogScope::Workspace {
                workspace_id: self.workspace.clone(),
            },
            media_type: self.filter.media_type().map(str::to_owned),
            through_sequence,
            after,
            limit: 20,
        })
    }

    pub(super) fn paste(&self, input: &mut Editor, value: &str) {
        if self.saving {
            let _ = input.insert(&clean(value).replace('\n', " "), 4096);
        }
    }
}

impl App {
    pub(super) fn open_deliverables(&mut self, worker: &Worker) {
        let Some(workspace) = self.workspace.as_ref() else {
            return;
        };
        let catalog = Catalog {
            workspace: workspace.id.clone(),
            filter: Filter::All,
            trail: vec![0],
            page: None,
            index: 0,
            preview: None,
            scroll: 0,
            saving: false,
        };
        if self.send(worker, catalog.request(0, None)) {
            self.message.clear();
            self.layer = Layer::Deliverables(Box::new(catalog));
        }
    }

    pub(super) fn install_deliverables(
        &mut self,
        workspace: &str,
        page: ArtifactCatalogSnapshotPage,
    ) {
        if let Layer::Deliverables(catalog) = &mut self.layer
            && catalog.workspace == workspace
        {
            catalog.index = 0;
            catalog.page = Some(page);
        }
    }

    pub(super) fn install_catalog_preview(&mut self, preview: Box<Preview>) {
        if let Layer::Deliverables(catalog) = &mut self.layer {
            catalog.preview = Some(preview);
            catalog.scroll = 0;
            catalog.saving = false;
        }
    }

    pub(super) fn catalog_key(&mut self, key: KeyEvent, worker: &Worker) {
        let Layer::Deliverables(mut catalog) = std::mem::replace(&mut self.layer, Layer::None)
        else {
            return;
        };
        let pressed = key.kind == KeyEventKind::Press;
        if !self.busy && pressed && key.code == KeyCode::Esc {
            self.message.clear();
            if catalog.saving {
                catalog.saving = false;
            } else if catalog.preview.take().is_none() {
                return;
            }
        } else if !self.busy && catalog.saving {
            if let Some(preview) = &catalog.preview {
                match key.code {
                    KeyCode::Enter if pressed && !self.input.text().trim().is_empty() => {
                        if self.send(
                            worker,
                            Request::Download(preview.id.clone(), self.input.text().trim().into()),
                        ) {
                            catalog.saving = false;
                            self.message.clear();
                        }
                    }
                    KeyCode::Char(character)
                        if !character.is_control()
                            && !key
                                .modifiers
                                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                    {
                        let _ = self.input.insert(character.encode_utf8(&mut [0; 4]), 4096);
                    }
                    code => {
                        self.input.key(code);
                    }
                }
            }
        } else if !self.busy
            && let Some(preview) = &catalog.preview
        {
            scroll_key(key.code, &mut catalog.scroll);
            match key.code {
                KeyCode::Char('d') if pressed => {
                    self.input.clear();
                    self.message.clear();
                    catalog.saving = true;
                }
                KeyCode::Char('p') if pressed => {
                    if let Some(revision) = &preview.revision {
                        self.queue_catalog_preview(worker, revision.previous.clone());
                    }
                }
                _ => {}
            }
        } else if !self.busy
            && let Some(page) = &catalog.page
        {
            step(key.code, &mut catalog.index, page.items.len());
            if pressed {
                match key.code {
                    KeyCode::Enter => {
                        if let Some(artifact) = page.items.get(catalog.index) {
                            self.queue_catalog_preview(worker, artifact_id(artifact));
                        }
                    }
                    KeyCode::Char('f') | KeyCode::Char('r') => {
                        if key.code == KeyCode::Char('f') {
                            catalog.filter = catalog.filter.next();
                        }
                        if self.send(worker, catalog.request(0, None)) {
                            catalog.trail = vec![0];
                            catalog.page = None;
                        }
                    }
                    KeyCode::PageDown if page.has_more => {
                        let next = page.next_after;
                        if self.send(worker, catalog.request(next, Some(page.through_sequence))) {
                            catalog.trail.push(next);
                        }
                    }
                    KeyCode::PageUp if catalog.trail.len() > 1 => {
                        let previous = catalog.trail[catalog.trail.len() - 2];
                        if self.send(
                            worker,
                            catalog.request(previous, Some(page.through_sequence)),
                        ) {
                            catalog.trail.pop();
                        }
                    }
                    _ => {}
                }
            }
        }
        self.layer = Layer::Deliverables(catalog);
    }

    fn queue_catalog_preview(&mut self, worker: &Worker, id: ArtifactGetParams) {
        if self.send(worker, Request::Preview(id.clone())) {
            self.pending_preview = Some(id);
            self.message.clear();
        }
    }

    pub(super) fn draw_deliverables(&self, frame: &mut Frame<'_>, area: Rect, catalog: &Catalog) {
        let [body, notice] = Layout::vertical([
            Constraint::Min(0),
            Constraint::Length(u16::from(!self.message.is_empty()) * 3),
        ])
        .areas(area);
        render_text(frame, notice, &clean(&self.message), 0);
        if catalog.saving {
            let [title, input] =
                Layout::vertical([Constraint::Length(2), Constraint::Min(0)]).areas(body);
            render_text(frame, title, "Destination file path", 0);
            if !input.is_empty() {
                let (lines, x, y) = self.input.viewport(input.width, input.height);
                frame.render_widget(
                    Paragraph::new(lines.into_iter().map(Line::raw).collect::<Vec<_>>()),
                    input,
                );
                frame.set_cursor_position(Position::new(input.x + x, input.y + y));
            }
            return;
        }
        if let Some(preview) = &catalog.preview {
            let revision = preview
                .revision
                .as_ref()
                .map_or_else(String::new, |revision| {
                    format!(
                        "\nEarlier revision: {}\nPrevious SHA-256: {}",
                        clean(&revision.previous.artifact_id),
                        revision.previous_digest.sha256
                    )
                });
            let text = format!(
                "{}\n{}  ·  {} B\nSHA-256: {}{revision}\n\n{}",
                clean(&preview.summary.title),
                clean(&preview.summary.media_type),
                preview.digest.content_bytes,
                preview.digest.sha256,
                preview
                    .text
                    .as_deref()
                    .unwrap_or("Download to view this deliverable.")
            );
            render_text(frame, body, &text, catalog.scroll);
            return;
        }
        let [header, list] =
            Layout::vertical([Constraint::Length(2), Constraint::Min(0)]).areas(body);
        let filter = catalog.filter.media_type().unwrap_or("all types");
        let title = catalog.page.as_ref().map_or_else(
            || format!("{filter} · Loading…"),
            |page| {
                format!(
                    "{filter} · Page {} · Through #{}",
                    catalog.trail.len(),
                    page.through_sequence
                )
            },
        );
        render_text(frame, header, &title, 0);
        if let Some(page) = &catalog.page {
            let rows = page
                .items
                .iter()
                .map(|item| {
                    if list.width >= 78 {
                        format!(
                            "{}  ·  {}  ·  {} B",
                            clean(&item.title),
                            clean(&item.media_type),
                            item.content_bytes
                        )
                    } else {
                        clean(&item.title)
                    }
                })
                .collect::<Vec<_>>();
            if rows.is_empty() {
                render_text(frame, list, "No deliverables match", 0);
            } else {
                render_list(frame, list, rows, catalog.index, self.palette.choice);
            }
        }
    }
}
