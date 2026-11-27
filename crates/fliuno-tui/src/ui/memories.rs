use super::*;
use fliuno_protocol::{
    ContextMemoryRef, MAX_EXPLICIT_CONTEXT_MEMORIES, MAX_WORKSPACE_MEMORY_CONTENT_BYTES,
    MAX_WORKSPACE_MEMORY_KEY_CHARS, MAX_WORKSPACE_MEMORY_QUERY_CHARS, WorkspaceMemory,
    WorkspaceMemoryPutParams,
};

pub(super) struct MemoryPanel {
    workspace: String,
    picks: Option<Vec<ContextMemoryRef>>,
    query: String,
    page: ObjectPage<WorkspaceMemory>,
    index: usize,
    view: MemoryView,
}

enum MemoryView {
    List,
    Search(Editor),
    Detail(u16),
    Key(Editor),
    Content {
        input: Editor,
        record: WorkspaceMemoryPutParams,
    },
}

impl MemoryView {
    fn editor(&mut self) -> Option<(&mut Editor, usize)> {
        match self {
            Self::Search(input) => Some((input, MAX_WORKSPACE_MEMORY_QUERY_CHARS * 4)),
            Self::Key(input) => Some((input, MAX_WORKSPACE_MEMORY_KEY_CHARS * 4)),
            Self::Content { input, .. } => Some((input, MAX_WORKSPACE_MEMORY_CONTENT_BYTES)),
            _ => None,
        }
    }
}

impl MemoryPanel {
    pub(super) fn help(&self) -> &'static str {
        match &self.view {
            MemoryView::List if self.picks.is_some() => {
                "Space Select  ·  Ctrl+S Apply  ·  f Find  ·  PgDn More  ·  Esc Back"
            }
            MemoryView::List => {
                "Enter Read  ·  n Add  ·  f Find  ·  PgDn More  ·  Home First  ·  Esc Back"
            }
            MemoryView::Detail(_) if self.picks.is_none() => "e Edit  ·  ↑/↓ Scroll  ·  Esc List",
            MemoryView::Detail(_) => "↑/↓ Scroll  ·  Esc List",
            MemoryView::Search(_) | MemoryView::Key(_) => "Enter Continue  ·  Esc List",
            MemoryView::Content { .. } => "Ctrl+S Save  ·  Esc List",
        }
    }

    fn list_request(&self, after: u64) -> Request {
        Request::Memories {
            workspace: self.workspace.clone(),
            query: self.query.clone(),
            after,
        }
    }

    fn selected(&self) -> Option<&WorkspaceMemory> {
        self.page.items.get(self.index)
    }

    pub(super) fn paste(&mut self, value: &str) {
        if let Some((input, limit)) = self.view.editor() {
            let value = clean(value);
            let single = !matches!(limit, MAX_WORKSPACE_MEMORY_CONTENT_BYTES);
            let value = if single {
                value.replace('\n', " ")
            } else {
                value
            };
            let _ = input.insert(&value, limit);
        }
    }
}

impl App {
    pub(super) fn open_memories(&mut self, worker: &Worker, select: bool) {
        if select && self.draft_context.mode != ContextMode::Session {
            self.message = "Switch to session context to select memories".into();
            return;
        }
        let Some(workspace) = self.workspace.as_ref() else {
            return;
        };
        let panel = MemoryPanel {
            workspace: workspace.id.clone(),
            picks: select.then(|| {
                self.draft_context
                    .memory_selection
                    .clone()
                    .unwrap_or_default()
            }),
            query: String::new(),
            page: ObjectPage {
                items: Vec::new(),
                next_after: 0,
                has_more: false,
            },
            index: 0,
            view: MemoryView::List,
        };
        if self.send(worker, panel.list_request(0)) {
            self.message.clear();
            self.layer = Layer::Memories(Box::new(panel));
        }
    }

    pub(super) fn install_memories(
        &mut self,
        workspace: &str,
        query: &str,
        page: ObjectPage<WorkspaceMemory>,
    ) {
        if let Layer::Memories(panel) = &mut self.layer
            && panel.workspace == workspace
            && panel.query == query
        {
            panel.index = 0;
            panel.page = page;
        }
    }

    pub(super) fn memory_saved(&mut self, workspace: &str) {
        if let Layer::Memories(panel) = &mut self.layer
            && panel.workspace == workspace
        {
            panel.view = MemoryView::List;
            self.next = Some(panel.list_request(0));
            self.message = "Memory saved".into();
        }
    }

    pub(super) fn memories_key(&mut self, key: KeyEvent, worker: &Worker) {
        let Layer::Memories(mut panel) = std::mem::replace(&mut self.layer, Layer::None) else {
            return;
        };
        let pressed = key.kind == KeyEventKind::Press;
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if key.code == KeyCode::Esc && pressed && !self.busy {
            if matches!(panel.view, MemoryView::List) {
                self.message.clear();
                if panel.picks.is_some() {
                    self.layer = Layer::Edit(Edit::Task);
                }
                return;
            }
            panel.view = MemoryView::List;
            self.message.clear();
        } else if !self.busy {
            match &mut panel.view {
                MemoryView::List => {
                    step(key.code, &mut panel.index, panel.page.items.len());
                    if pressed {
                        match key.code {
                            KeyCode::Char('f') => {
                                let mut input = Editor::default();
                                let _ = input
                                    .insert(&panel.query, MAX_WORKSPACE_MEMORY_QUERY_CHARS * 4);
                                panel.view = MemoryView::Search(input);
                            }
                            KeyCode::Char('n') if panel.picks.is_none() => {
                                panel.view = MemoryView::Key(Editor::default())
                            }
                            KeyCode::Enter if panel.selected().is_some() => {
                                panel.view = MemoryView::Detail(0)
                            }
                            KeyCode::PageDown if panel.page.has_more => {
                                self.send(worker, panel.list_request(panel.page.next_after));
                            }
                            KeyCode::Home | KeyCode::Char('r') => {
                                self.send(worker, panel.list_request(0));
                            }
                            KeyCode::Char(' ') if panel.picks.is_some() => {
                                if let Some(memory) = panel.selected() {
                                    let selected = ContextMemoryRef {
                                        id: memory.id.clone(),
                                        revision: memory.revision,
                                    };
                                    let picks = panel.picks.as_mut().expect("selection mode");
                                    if let Some(index) =
                                        picks.iter().position(|item| item.id == selected.id)
                                    {
                                        if picks[index].revision == selected.revision {
                                            picks.remove(index);
                                        } else {
                                            picks[index] = selected;
                                        }
                                    } else if picks.len() < MAX_EXPLICIT_CONTEXT_MEMORIES {
                                        picks.push(selected);
                                    } else {
                                        self.message = "Select up to eight memories".into();
                                    }
                                }
                            }
                            KeyCode::Char('s') if ctrl && panel.picks.is_some() => {
                                let picks = panel.picks.take().expect("selection mode");
                                if !picks.is_empty() {
                                    self.draft_context.disable_memory = false;
                                    self.draft_context.memory_recency_half_life_days = None;
                                }
                                self.draft_context.memory_selection =
                                    (!picks.is_empty()).then_some(picks);
                                self.key = uuid::Uuid::new_v4().to_string();
                                self.message.clear();
                                self.layer = Layer::Edit(Edit::Task);
                                return;
                            }
                            _ => {}
                        }
                    }
                }
                MemoryView::Detail(scroll) => {
                    scroll_key(key.code, scroll);
                    if key.code == KeyCode::Char('e')
                        && pressed
                        && panel.picks.is_none()
                        && let Some(memory) = panel.selected()
                    {
                        let mut input = Editor::default();
                        let _ = input.insert(&memory.content, MAX_WORKSPACE_MEMORY_CONTENT_BYTES);
                        panel.view = MemoryView::Content {
                            input,
                            record: WorkspaceMemoryPutParams {
                                workspace_id: memory.workspace_id.clone(),
                                key: memory.key.clone(),
                                content: String::new(),
                                source_task_id: memory.source_task_id.clone(),
                                expected_revision: Some(memory.revision),
                            },
                        };
                        self.message.clear();
                    }
                }
                MemoryView::Search(input) if pressed && key.code == KeyCode::Enter => {
                    let query = input.text().trim();
                    if query.chars().count() <= MAX_WORKSPACE_MEMORY_QUERY_CHARS {
                        panel.query = query.to_owned();
                        panel.view = MemoryView::List;
                        self.message.clear();
                        self.send(worker, panel.list_request(0));
                    } else {
                        self.message = "Search query is too long".into();
                    }
                }
                MemoryView::Key(input) if pressed && key.code == KeyCode::Enter => {
                    let key = input.text().trim();
                    if !key.is_empty() && key.chars().count() <= MAX_WORKSPACE_MEMORY_KEY_CHARS {
                        panel.view = MemoryView::Content {
                            input: Editor::default(),
                            record: WorkspaceMemoryPutParams {
                                workspace_id: panel.workspace.clone(),
                                key: key.to_owned(),
                                content: String::new(),
                                source_task_id: None,
                                expected_revision: None,
                            },
                        };
                        self.message.clear();
                    } else {
                        self.message = "Enter a memory key of up to 128 characters".into();
                    }
                }
                MemoryView::Content { input, record }
                    if pressed && ctrl && key.code == KeyCode::Char('s') =>
                {
                    if input.text().trim().is_empty() {
                        self.message = "Enter the memory text".into();
                    } else {
                        let mut record = record.clone();
                        record.content = input.text().to_owned();
                        self.message.clear();
                        self.send(worker, Request::SaveMemory(record));
                    }
                }
                view => {
                    if let Some((input, limit)) = view.editor() {
                        match key.code {
                            KeyCode::Char(c)
                                if !ctrl
                                    && !c.is_control()
                                    && !key.modifiers.contains(KeyModifiers::ALT) =>
                            {
                                let _ = input.insert(c.encode_utf8(&mut [0; 4]), limit);
                            }
                            KeyCode::Enter if limit == MAX_WORKSPACE_MEMORY_CONTENT_BYTES => {
                                let _ = input.insert("\n", limit);
                            }
                            code => {
                                input.key(code);
                            }
                        }
                    }
                }
            }
        }
        self.layer = Layer::Memories(panel);
    }

    pub(super) fn draw_memories(&self, frame: &mut Frame<'_>, area: Rect, panel: &MemoryPanel) {
        let [body, notice] = Layout::vertical([
            Constraint::Min(0),
            Constraint::Length(u16::from(!self.message.is_empty()) * 2),
        ])
        .areas(area);
        render_text(frame, notice, &clean(&self.message), 0);
        match &panel.view {
            MemoryView::List => {
                let [subtitle, list] =
                    Layout::vertical([Constraint::Length(2), Constraint::Min(0)]).areas(body);
                let title = match (&panel.picks, panel.query.is_empty()) {
                    (Some(picks), _) => format!(
                        "{} of {} selected for this draft",
                        picks.len(),
                        MAX_EXPLICIT_CONTEXT_MEMORIES
                    ),
                    (_, false) => format!("Results for {}", clean(&panel.query)),
                    _ => "Saved workspace memories".into(),
                };
                render_text(frame, subtitle, &title, 0);
                let rows = panel
                    .page
                    .items
                    .iter()
                    .map(|memory| {
                        let selected = panel.picks.as_ref().is_some_and(|picks| {
                            picks.iter().any(|item| {
                                item.id == memory.id && item.revision == memory.revision
                            })
                        });
                        let mark = if selected { "✓ " } else { "" };
                        format!("{mark}{}  ·  v{}", clean(&memory.key), memory.revision)
                    })
                    .collect::<Vec<_>>();
                if rows.is_empty() {
                    let message = if self.busy {
                        "Loading memories…"
                    } else {
                        "No memories match"
                    };
                    render_text(frame, list, message, 0);
                } else {
                    render_list(frame, list, rows, panel.index, self.palette.choice);
                }
            }
            MemoryView::Detail(scroll) => {
                if let Some(memory) = panel.selected() {
                    let text = format!(
                        "{}\nRevision {}\nSource: {}\n\n{}",
                        clean(&memory.key),
                        memory.revision,
                        memory
                            .source_task_id
                            .as_deref()
                            .map(clean)
                            .unwrap_or_else(|| "Manual entry".into()),
                        clean(&memory.content)
                    );
                    render_text(frame, body, &text, *scroll);
                }
            }
            view => {
                let (title, input) = match view {
                    MemoryView::Search(input) => ("Find memories by key or text".into(), input),
                    MemoryView::Key(input) => ("Memory key".into(), input),
                    MemoryView::Content { input, record } => (
                        match record.expected_revision {
                            Some(revision) => {
                                format!("Editing {} · revision {}", clean(&record.key), revision)
                            }
                            None => format!("New memory · {}", clean(&record.key)),
                        },
                        input,
                    ),
                    _ => unreachable!(),
                };
                let [title_area, editor] =
                    Layout::vertical([Constraint::Length(2), Constraint::Min(0)]).areas(body);
                render_text(frame, title_area, &title, 0);
                if !editor.is_empty() {
                    let (lines, x, y) = input.viewport(editor.width, editor.height);
                    frame.render_widget(
                        Paragraph::new(lines.into_iter().map(Line::raw).collect::<Vec<_>>()),
                        editor,
                    );
                    frame.set_cursor_position(Position::new(editor.x + x, editor.y + y));
                }
            }
        }
    }
}
