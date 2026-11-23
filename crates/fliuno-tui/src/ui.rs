use crate::{
    editor::Editor,
    options::Theme,
    service::{Detail, Preview, Request, Update, Worker},
};
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use fliuno_protocol::{
    ApprovalDecision, ApprovalDetails, ArtifactGetParams, ArtifactSummary, Message, ObjectPage,
    Session, TaskResumeDisposition, TaskStatus, WorkActionKind, WorkSnapshot, Workspace,
};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Margin, Position, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap},
};
use unicode_segmentation::UnicodeSegmentation;

const ACCENT: Color = Color::Cyan;
const DRAFT_LIMIT: usize = 48 * 1024;

struct Palette {
    base: Style,
    rail: Style,
    label: Style,
    quiet: Style,
    choice: Style,
    mono: bool,
}

impl Palette {
    fn new(theme: Theme) -> Self {
        let bold = |style: Style| style.add_modifier(Modifier::BOLD);
        match theme {
            Theme::Dark | Theme::Light => {
                let (bg, fg, layer, accent, quiet, selected) = if matches!(theme, Theme::Dark) {
                    (
                        (17, 20, 22),
                        (228, 231, 228),
                        (32, 38, 40),
                        (132, 224, 180),
                        (166, 177, 175),
                        (37, 68, 57),
                    )
                } else {
                    (
                        (249, 250, 247),
                        (29, 37, 33),
                        (230, 236, 230),
                        (13, 100, 77),
                        (75, 93, 86),
                        (205, 229, 217),
                    )
                };
                let rgb = |(r, g, b)| Color::Rgb(r, g, b);
                let base = Style::default().fg(rgb(fg)).bg(rgb(bg));
                Self {
                    base,
                    rail: base.bg(rgb(layer)),
                    label: bold(Style::default().fg(rgb(accent))),
                    quiet: Style::default().fg(rgb(quiet)),
                    choice: bold(base.bg(rgb(selected))),
                    mono: true,
                }
            }
            Theme::Auto | Theme::Mono => {
                let mono = matches!(theme, Theme::Mono);
                let base = Style::default();
                Self {
                    base,
                    rail: base,
                    label: bold(if mono { base } else { base.fg(Color::Cyan) }),
                    quiet: base.add_modifier(Modifier::DIM),
                    choice: bold(base.add_modifier(Modifier::REVERSED)),
                    mono,
                }
            }
        }
    }

    fn status(&self, color: Color) -> Style {
        if self.mono {
            self.label
        } else {
            self.label.fg(color)
        }
    }
}

enum Layer {
    Commands(usize, String),
    Workspaces(Option<ObjectPage<Workspace>>, usize),
    Sessions(Option<ObjectPage<Session>>, usize),
    Artifacts(usize),
    Approval {
        task: String,
        id: String,
        text: String,
        scroll: u16,
    },
    Preview(Box<Preview>, u16),
    ConfirmCancel(String),
    Edit(Edit),
    None,
}

#[derive(Clone)]
enum Edit {
    Task,
    Workspace,
    Session,
    Answer(String, String, String),
    Steer(String),
    Download(ArtifactGetParams),
}

impl Edit {
    fn name(&self) -> &str {
        match self {
            Self::Task => "New task",
            Self::Workspace => "Open folder",
            Self::Session => "New session",
            Self::Answer(_, _, _) => "Answer clarification",
            Self::Steer(_) => "Add instruction",
            Self::Download(_) => "Save artifact to",
        }
    }
}

pub struct App {
    demo: bool,
    palette: Palette,
    online: bool,
    busy: bool,
    next: Option<Request>,
    workspace: Option<Workspace>,
    session: Option<Session>,
    snapshot: Option<WorkSnapshot>,
    selected: Option<String>,
    detail: Option<Box<Detail>>,
    scroll: u16,
    focus_details: bool,
    layer: Layer,
    draft: Editor,
    parked: Vec<(String, Editor, String, Option<ArtifactGetParams>)>,
    key: String,
    input: Editor,
    input_artifact: Option<ArtifactGetParams>,
    pending_preview: Option<ArtifactGetParams>,
    start_pending: bool,
    start_acknowledged: bool,
    message: String,
}

impl App {
    pub fn new(demo: bool, theme: Theme) -> Self {
        Self {
            demo,
            palette: Palette::new(theme),
            online: false,
            busy: true,
            next: None,
            workspace: None,
            session: None,
            snapshot: None,
            selected: None,
            detail: None,
            scroll: 0,
            focus_details: false,
            layer: Layer::None,
            draft: Editor::default(),
            parked: Vec::new(),
            key: uuid::Uuid::new_v4().to_string(),
            input: Editor::default(),
            input_artifact: None,
            pending_preview: None,
            start_pending: false,
            start_acknowledged: false,
            message: "Connecting".into(),
        }
    }

    fn tasks(&self) -> &[fliuno_protocol::TaskSummary] {
        self.snapshot
            .as_ref()
            .and_then(|snap| snap.recent_tasks.as_deref())
            .unwrap_or_default()
    }

    fn send(&mut self, worker: &Worker, request: Request) -> bool {
        if self.busy || !worker.send(request) {
            return false;
        }
        self.busy = true;
        true
    }

    fn sync(&mut self, worker: &Worker) {
        if !self.busy
            && let Some(next) = self.next.take()
            && !self.send(worker, next)
        {
            self.message = "Connection worker unavailable".into();
        }
    }

    pub fn drain(&mut self, worker: &Worker) -> bool {
        let mut changed = false;
        for _ in 0..16 {
            let Ok(update) = worker.updates.try_recv() else {
                break;
            };
            self.update(update);
            changed = true;
        }
        self.sync(worker);
        changed
    }

    fn select_session(&mut self, session: Session) {
        if self.session.as_ref().map(|current| &current.id) != Some(&session.id) {
            self.park_draft();
            if let Some(i) = self.parked.iter().position(|(id, ..)| *id == session.id) {
                let (_, draft, key, artifact) = self.parked.remove(i);
                self.draft = draft;
                self.key = key;
                self.input_artifact = artifact;
            }
        }
        self.next = Some(Request::SelectSession(session.id.clone()));
        self.session = Some(session);
        self.snapshot = None;
        self.selected = None;
        self.detail = None;
        self.scroll = 0;
        self.layer = Layer::None;
    }

    fn park_draft(&mut self) {
        let draft = std::mem::take(&mut self.draft);
        let artifact = self.input_artifact.take();
        let key = std::mem::replace(&mut self.key, uuid::Uuid::new_v4().to_string());
        if let Some(session) = &self.session
            && (!draft.text().is_empty() || artifact.is_some())
        {
            if self.parked.len() >= 8 {
                self.parked.remove(0);
            }
            self.parked.push((session.id.clone(), draft, key, artifact));
        }
    }

    fn select_task(&mut self, id: String) {
        self.next = Some(Request::Task(id.clone()));
        self.selected = Some(id);
        self.detail = None;
        self.scroll = 0;
    }

    fn update(&mut self, update: Update) {
        match update {
            Update::Online(initial) => {
                self.online = true;
                self.message.clear();
                let id = self.workspace.get_or_insert(initial).id.clone();
                self.next = Some(match &self.session {
                    Some(session) => Request::SelectSession(session.id.clone()),
                    None => sessions(id, 0, false),
                });
            }
            Update::Workspace(workspace) => {
                self.park_draft();
                self.next = Some(sessions(workspace.id.clone(), 0, false));
                self.workspace = Some(workspace);
                self.session = None;
                self.snapshot = None;
                self.selected = None;
                self.detail = None;
                self.input_artifact = None;
                self.layer = Layer::None;
            }
            Update::Workspaces(page) => {
                if matches!(self.layer, Layer::Workspaces(..)) {
                    self.layer = Layer::Workspaces(Some(page), 0);
                }
            }
            Update::Sessions {
                workspace,
                page,
                browse,
            } => {
                if self
                    .workspace
                    .as_ref()
                    .is_none_or(|root| root.id != workspace)
                {
                    return;
                }
                if browse {
                    if matches!(self.layer, Layer::Sessions(..)) {
                        self.layer = Layer::Sessions(Some(page), 0);
                    }
                } else if let Some(session) = page.items.into_iter().next() {
                    self.select_session(session);
                } else {
                    self.layer = Layer::Edit(Edit::Session);
                }
            }
            Update::Session(session) => {
                if self
                    .workspace
                    .as_ref()
                    .is_some_and(|root| root.id == session.workspace_id)
                {
                    self.select_session(session);
                }
            }
            Update::Overview(session, snapshot) => {
                if self
                    .session
                    .as_ref()
                    .is_none_or(|active| active.id != session)
                {
                    return;
                }
                let previous = self
                    .snapshot
                    .as_ref()
                    .map(|snap| snap.approvals.snapshot.through_sequence);
                let current = snapshot.approvals.snapshot.through_sequence;
                self.snapshot = Some(snapshot);
                if self.selected.is_none() {
                    if let Some(task) = self.tasks().first() {
                        self.select_task(task.id.clone());
                    }
                } else if previous != Some(current)
                    && let Some(id) = &self.selected
                {
                    self.next = Some(Request::Task(id.clone()));
                }
            }
            Update::Detail(detail) => {
                if self
                    .session
                    .as_ref()
                    .is_some_and(|session| session.id == detail.task.session_id)
                    && (self.start_acknowledged
                        || self.selected.as_deref() == Some(detail.task.id.as_str()))
                {
                    if self.start_acknowledged {
                        self.selected = Some(detail.task.id.clone());
                    }
                    self.detail = Some(detail);
                }
            }
            Update::Approval(task, id, details) => {
                if self.selected.as_deref() == Some(task.as_str())
                    && matches!(self.layer, Layer::None)
                    && let ApprovalDetails::ToolCall { approval } = *details
                    && approval.task_id == task
                    && approval.id == id
                {
                    let arguments =
                        serde_json::to_string_pretty(&approval.call.arguments).unwrap_or_default();
                    let review = approval
                        .execution_plan
                        .as_ref()
                        .and_then(|plan| serde_json::to_string_pretty(plan).ok())
                        .unwrap_or_default();
                    let root = self
                        .workspace
                        .as_ref()
                        .map(|workspace| workspace.root.as_str())
                        .unwrap_or_default();
                    let text = format!(
                        "{}\nWorkspace: {root}\n\n{arguments}\n\n{review}",
                        approval.call.name
                    );
                    self.layer = Layer::Approval {
                        task,
                        id,
                        text,
                        scroll: 0,
                    };
                }
            }
            Update::Preview(preview) => {
                if matches!(self.layer, Layer::Artifacts(_))
                    && self.pending_preview.take().is_some_and(|expected| {
                        expected.task_id == preview.id.task_id
                            && expected.artifact_id == preview.id.artifact_id
                    })
                {
                    self.layer = Layer::Preview(preview, 0);
                }
            }
            Update::Started(id) => {
                self.start_acknowledged = true;
                self.selected = Some(id);
                self.draft.clear();
                self.input_artifact = None;
                self.key = uuid::Uuid::new_v4().to_string();
                self.layer = Layer::None;
                self.scroll = 0;
                self.message = "Task submitted".into();
            }
            Update::Notice(message) | Update::Error(message) => self.message = message,
            Update::Offline(message) => {
                self.online = false;
                self.layer = Layer::None;
                self.next = None;
                self.message = message;
            }
            Update::Finished => {
                self.busy = false;
                self.start_pending = false;
                self.start_acknowledged = false;
            }
        }
    }

    pub fn paste(&mut self, value: &str) {
        if self.busy && self.start_pending {
            return;
        }
        if let Layer::Edit(edit) = &self.layer {
            let text = clean(value);
            let target = if matches!(edit, Edit::Task) {
                &mut self.draft
            } else {
                &mut self.input
            };
            if target.insert(&text, edit_limit(edit)) && matches!(edit, Edit::Task) {
                self.key = uuid::Uuid::new_v4().to_string();
            }
        } else if let Layer::Commands(index, query) = &mut self.layer {
            push_bounded(query, &clean(value).replace('\n', " "), 128);
            *index = 0;
        }
    }

    pub fn key(&mut self, key: KeyEvent, worker: &Worker) -> bool {
        if key.kind == KeyEventKind::Release {
            return false;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let pressed = key.kind == KeyEventKind::Press;
        if ctrl && key.code == KeyCode::Char('c') {
            return true;
        }
        if key.code == KeyCode::Esc && !matches!(self.layer, Layer::None) {
            if !self.start_pending {
                self.layer = Layer::None;
            }
            return false;
        }
        if let Layer::Edit(edit) = &self.layer {
            if self.start_pending {
                return false;
            }
            let edit = edit.clone();
            let value = if matches!(edit, Edit::Task) {
                &mut self.draft
            } else {
                &mut self.input
            };
            if pressed
                && ((ctrl && key.code == KeyCode::Char('s'))
                    || (key.code == KeyCode::Enter && !matches!(edit, Edit::Task)))
            {
                self.submit(edit, worker);
                return false;
            }
            let changed = match key.code {
                KeyCode::Enter if matches!(edit, Edit::Task) => {
                    value.insert("\n", edit_limit(&edit))
                }
                KeyCode::Char(c)
                    if !ctrl && !key.modifiers.contains(KeyModifiers::ALT) && !c.is_control() =>
                {
                    value.insert(c.encode_utf8(&mut [0; 4]), edit_limit(&edit))
                }
                code => value.key(code),
            };
            if changed && matches!(edit, Edit::Task) {
                self.key = uuid::Uuid::new_v4().to_string();
            }
            return false;
        }

        let mut request = None;
        let mut close = false;
        if matches!(self.layer, Layer::Commands(..)) {
            let Layer::Commands(mut index, mut query) =
                std::mem::replace(&mut self.layer, Layer::None)
            else {
                unreachable!()
            };
            match key.code {
                KeyCode::Backspace => {
                    if let Some((offset, _)) = query.grapheme_indices(true).next_back() {
                        query.truncate(offset);
                    }
                    index = 0;
                }
                KeyCode::Char(c) if !ctrl && !c.is_control() => {
                    push_bounded(&mut query, c.encode_utf8(&mut [0; 4]), 128);
                    index = 0;
                }
                _ => {}
            }
            let commands = self.filtered_commands(&query);
            index = index.min(commands.len().saturating_sub(1));
            step(key.code, &mut index, commands.len());
            if pressed
                && key.code == KeyCode::Enter
                && let Some(&(_, command)) = commands.get(index)
            {
                return self.overview_key(KeyEvent::new(command, KeyModifiers::NONE), worker);
            }
            self.layer = Layer::Commands(index, query);
            return false;
        }
        match &mut self.layer {
            Layer::Workspaces(page, index) => {
                if pressed && key.code == KeyCode::Char('o') {
                    self.input.clear();
                    self.layer = Layer::Edit(Edit::Workspace);
                    return false;
                }
                if let Some(page) = page {
                    step(key.code, index, page.items.len());
                    if pressed
                        && key.code == KeyCode::Enter
                        && let Some(value) = page.items.get(*index)
                    {
                        request = Some(Request::OpenWorkspace(value.root.clone()));
                    } else if pressed && key.code == KeyCode::PageDown && page.has_more {
                        request = Some(Request::Workspaces(page.next_after));
                    } else if pressed && key.code == KeyCode::PageUp {
                        request = Some(Request::Workspaces(0));
                    }
                }
            }
            Layer::Sessions(page, index) => {
                if pressed && key.code == KeyCode::Char('n') {
                    self.input.clear();
                    self.layer = Layer::Edit(Edit::Session);
                    return false;
                }
                if let (Some(page), Some(root)) = (page, &self.workspace) {
                    step(key.code, index, page.items.len());
                    if pressed
                        && !self.busy
                        && key.code == KeyCode::Enter
                        && let Some(value) = page.items.get(*index)
                    {
                        let selected = value.clone();
                        self.select_session(selected);
                        self.sync(worker);
                        return false;
                    } else if pressed && key.code == KeyCode::PageDown && page.has_more {
                        request = Some(sessions(root.id.clone(), page.next_after, true));
                    } else if pressed && key.code == KeyCode::PageUp {
                        request = Some(sessions(root.id.clone(), 0, true));
                    }
                }
            }
            Layer::Artifacts(index) => {
                if let Some(detail) = &self.detail {
                    step(key.code, index, detail.artifacts.len());
                    if key.code == KeyCode::Enter
                        && pressed
                        && let Some(value) = detail.artifacts.get(*index)
                    {
                        let id = artifact_id(value);
                        self.pending_preview = Some(id.clone());
                        request = Some(Request::Preview(id));
                    }
                }
            }
            Layer::Approval {
                task, id, scroll, ..
            } => {
                scroll_key(key.code, scroll);
                if pressed {
                    let decision = match key.code {
                        KeyCode::Char('y') => Some(ApprovalDecision::AllowOnce),
                        KeyCode::Char('n') => Some(ApprovalDecision::Deny),
                        _ => None,
                    };
                    if let Some(decision) = decision {
                        request = Some(Request::Decide(task.clone(), id.clone(), decision));
                        close = true;
                    }
                }
            }
            Layer::Preview(preview, scroll) => {
                scroll_key(key.code, scroll);
                if pressed && key.code == KeyCode::Char('d') {
                    self.input.clear();
                    self.layer = Layer::Edit(Edit::Download(preview.id.clone()));
                } else if pressed && key.code == KeyCode::Char('u') && preview.text.is_some() {
                    self.input_artifact = Some(preview.id.clone());
                    self.key = uuid::Uuid::new_v4().to_string();
                    self.layer = Layer::Edit(Edit::Task);
                } else if pressed
                    && key.code == KeyCode::Char('p')
                    && let Some(revision) = &preview.revision
                {
                    let id = revision.previous.clone();
                    if self.send(worker, Request::Preview(id.clone())) {
                        self.pending_preview = Some(id);
                        self.layer = Layer::Artifacts(0);
                    }
                }
            }
            Layer::ConfirmCancel(id) => {
                if pressed && key.code == KeyCode::Char('y') {
                    request = Some(Request::Cancel(id.clone()));
                    close = true;
                }
            }
            Layer::None => return self.overview_key(key, worker),
            Layer::Commands(..) | Layer::Edit(..) => unreachable!(),
        }
        if let Some(request) = request
            && self.send(worker, request)
            && close
        {
            self.layer = Layer::None;
        }
        false
    }

    fn overview_key(&mut self, key: KeyEvent, worker: &Worker) -> bool {
        let pressed = key.kind == KeyEventKind::Press;
        if pressed
            && ((key.code == KeyCode::F(1))
                || (key.code == KeyCode::Char('/')
                    && !key.modifiers.contains(KeyModifiers::CONTROL)))
        {
            self.layer = Layer::Commands(0, String::new());
            return false;
        }
        if key.code == KeyCode::Char('q') && pressed {
            return true;
        }
        if key.code == KeyCode::Char('r') && pressed && !self.online {
            self.send(worker, Request::Reconnect);
            return false;
        }
        if !self.online {
            return false;
        }
        let active = !self.busy && pressed;
        match key.code {
            KeyCode::Tab if pressed => self.focus_details = !self.focus_details,
            KeyCode::Up | KeyCode::Down | KeyCode::PageUp | KeyCode::PageDown
                if self.focus_details =>
            {
                scroll_key(key.code, &mut self.scroll)
            }
            KeyCode::Up | KeyCode::Down => {
                let mut index = self
                    .tasks()
                    .iter()
                    .position(|task| Some(&task.id) == self.selected.as_ref())
                    .unwrap_or(0);
                step(key.code, &mut index, self.tasks().len());
                if let Some(id) = self.tasks().get(index).map(|task| task.id.clone())
                    && self.selected.as_ref() != Some(&id)
                {
                    self.select_task(id);
                }
            }
            KeyCode::Char('w') if active => {
                if self.send(worker, Request::Workspaces(0)) {
                    self.layer = Layer::Workspaces(None, 0);
                }
            }
            KeyCode::Char('s') if active => {
                if let Some(root) = &self.workspace
                    && self.send(worker, sessions(root.id.clone(), 0, true))
                {
                    self.layer = Layer::Sessions(None, 0);
                }
            }
            KeyCode::Char('n') if active => {
                self.layer = Layer::Edit(if self.session.is_some() {
                    Edit::Task
                } else {
                    self.input.clear();
                    Edit::Session
                })
            }
            KeyCode::Char('v') if active && self.detail.is_some() => {
                self.layer = Layer::Artifacts(0)
            }
            _ if active => self.task_key(key.code, worker),
            _ => {}
        }
        self.sync(worker);
        false
    }

    fn commands(&self) -> Vec<(&'static str, KeyCode)> {
        let mut commands = Vec::new();
        if self.online {
            commands.push((
                if self.session.is_some() {
                    "New task"
                } else {
                    "New session"
                },
                KeyCode::Char('n'),
            ));
            commands.push(("Workspaces", KeyCode::Char('w')));
            commands.push(("Sessions", KeyCode::Char('s')));
            if let Some(detail) = &self.detail {
                let task = &detail.task;
                if task.pending_approval.is_some() {
                    commands.push(("Review operation", KeyCode::Char('a')));
                } else if task
                    .pending_tool_input
                    .as_ref()
                    .is_some_and(|pending| pending.requests["kind"] == "clarification")
                {
                    commands.push(("Answer clarification", KeyCode::Char('a')));
                }
                if !task.status.is_terminal() {
                    if task.status != TaskStatus::Interrupted {
                        commands.push(("Add instruction", KeyCode::Char('i')));
                    }
                    commands.push(("Cancel task", KeyCode::Char('x')));
                }
                if detail
                    .resume
                    .as_ref()
                    .is_some_and(|status| status.disposition == TaskResumeDisposition::Ready)
                {
                    commands.push(("Resume task", KeyCode::Char('r')));
                }
                if !detail.artifacts.is_empty() {
                    commands.push(("Open artifacts", KeyCode::Char('v')));
                }
            }
        } else {
            commands.push(("Reconnect", KeyCode::Char('r')));
        }
        commands.push(("Quit", KeyCode::Char('q')));
        commands
    }

    fn filtered_commands(&self, query: &str) -> Vec<(&'static str, KeyCode)> {
        let query = query.to_lowercase();
        self.commands()
            .into_iter()
            .filter(|(label, _)| {
                let label = label.to_ascii_lowercase();
                query.split_whitespace().all(|term| label.contains(term))
            })
            .collect()
    }

    fn task_key(&mut self, key: KeyCode, worker: &Worker) {
        let Some(detail) = &self.detail else { return };
        let task = &detail.task;
        match key {
            KeyCode::Enter | KeyCode::Char('a') => {
                if let Some(approval) = &task.pending_approval {
                    self.send(
                        worker,
                        Request::Approval(task.id.clone(), approval.id.clone()),
                    );
                } else if let Some(pending) = &task.pending_tool_input
                    && pending.requests["kind"] == "clarification"
                    && let Some(question) = pending.requests["question"].as_str()
                {
                    self.input.clear();
                    self.layer = Layer::Edit(Edit::Answer(
                        task.id.clone(),
                        pending.call_id.clone(),
                        question.to_owned(),
                    ));
                } else {
                    self.focus_details = true;
                }
            }
            KeyCode::Char('x') if !task.status.is_terminal() => {
                self.layer = Layer::ConfirmCancel(task.id.clone())
            }
            KeyCode::Char('i')
                if !task.status.is_terminal() && task.status != TaskStatus::Interrupted =>
            {
                self.input.clear();
                self.layer = Layer::Edit(Edit::Steer(task.id.clone()));
            }
            KeyCode::Char('r')
                if detail
                    .resume
                    .as_ref()
                    .is_some_and(|status| status.disposition == TaskResumeDisposition::Ready) =>
            {
                self.send(worker, Request::Resume(task.id.clone()));
            }
            _ => {}
        }
    }

    fn submit(&mut self, edit: Edit, worker: &Worker) {
        let value = if matches!(edit, Edit::Task) {
            self.draft.text().trim()
        } else {
            self.input.text().trim()
        };
        if value.is_empty() {
            return;
        }
        let value = value.to_owned();
        let starts = matches!(edit, Edit::Task);
        let request = match edit {
            Edit::Task => {
                let Some(session) = &self.session else { return };
                Request::Start {
                    session: session.id.clone(),
                    text: value,
                    key: self.key.clone(),
                    input: self.input_artifact.clone(),
                }
            }
            Edit::Workspace => Request::OpenWorkspace(value),
            Edit::Session => {
                let Some(workspace) = &self.workspace else {
                    return;
                };
                Request::NewSession(workspace.id.clone(), value)
            }
            Edit::Answer(task, call, _) => Request::Answer(task, call, value),
            Edit::Steer(task) => Request::Steer(task, value),
            Edit::Download(id) => Request::Download(id, value.into()),
        };
        if self.send(worker, request) {
            if starts {
                self.start_pending = true;
            } else {
                self.layer = Layer::None;
            }
        }
    }

    pub fn draw(&self, frame: &mut Frame<'_>) {
        let area = frame.area();
        frame.render_widget(Block::default().style(self.palette.base), area);
        let expanded = matches!(self.layer, Layer::Edit(Edit::Task));
        let editor_height = if expanded {
            (area.height / 4).clamp(2, 7)
        } else {
            2
        };
        let [top, middle, composer, bottom] = Layout::vertical([
            Constraint::Length(if area.height >= 10 { 2 } else { 1 }),
            Constraint::Min(0),
            Constraint::Length(editor_height),
            Constraint::Length(1),
        ])
        .areas(area);
        let root = self
            .workspace
            .as_ref()
            .map(|root| clean(&root.root))
            .unwrap_or_default();
        let session = self
            .session
            .as_ref()
            .map(session_name)
            .unwrap_or_else(|| "No session".into());
        let state = if !self.online {
            "Offline"
        } else if self.demo {
            "Offline demo"
        } else {
            "Local"
        };
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(vec![
                    Span::styled(" FLIUNO ", self.palette.label),
                    Span::raw(format!("  {session}")),
                    Span::styled(format!("  ·  {state}"), self.palette.quiet),
                ]),
                Line::styled(format!(" {root}"), self.palette.quiet),
            ])
            .style(self.palette.rail),
            top,
        );
        if area.width >= 90 {
            let [tasks, details] = Layout::horizontal([
                Constraint::Length((area.width / 4).clamp(24, 34)),
                Constraint::Min(0),
            ])
            .areas(middle);
            self.draw_tasks(frame, tasks, false);
            self.draw_detail(frame, details.inner(Margin::new(2, 0)));
        } else if self.focus_details {
            self.draw_detail(frame, middle);
        } else {
            self.draw_tasks(frame, middle, true);
        }
        self.draw_composer(frame, composer, expanded);
        let message = if self.busy { "Working" } else { &self.message };
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(" F1 Commands  ", self.palette.label),
                Span::raw(clean(message).replace('\n', " ")),
            ]))
            .style(self.palette.rail),
            bottom,
        );
        self.draw_layer(frame, area);
    }

    fn draw_composer(&self, frame: &mut Frame<'_>, area: Rect, focused: bool) {
        if area.width < 4 || area.height == 0 {
            return;
        }
        let title = if let Some(artifact) = &self.input_artifact {
            format!(" New task  ·  Attached: {} ", clean(&artifact.artifact_id))
        } else if focused {
            " New task  ·  Ctrl+S submit ".to_owned()
        } else if self.session.is_none() {
            " No session ".to_owned()
        } else {
            " Draft  ·  N edit ".to_owned()
        };
        let block = Block::default()
            .borders(Borders::TOP)
            .style(self.palette.rail)
            .border_style(self.palette.quiet)
            .title(Span::styled(title, self.palette.label));
        let body = block.inner(area).inner(Margin::new(1, 0));
        frame.render_widget(block, area);
        if body.height == 0 || body.width == 0 {
            return;
        }
        let (lines, x, y) = self.draft.viewport(body.width, body.height);
        frame.render_widget(
            Paragraph::new(lines.into_iter().map(Line::raw).collect::<Vec<_>>()),
            body,
        );
        if focused && !self.start_pending {
            frame.set_cursor_position(Position::new(body.x + x, body.y + y));
        }
    }

    fn draw_tasks(&self, frame: &mut Frame<'_>, area: Rect, compact: bool) {
        let tasks = self.tasks();
        let title = if compact { "Task queue" } else { "Tasks" };
        let mut state = ListState::default().with_selected(
            tasks
                .iter()
                .position(|task| Some(&task.id) == self.selected.as_ref()),
        );
        let count = self
            .snapshot
            .as_ref()
            .map_or(0, |snap| snap.approvals.page.items.len());
        let list: Vec<_> = tasks
            .iter()
            .map(|task| {
                let flag = if task.agent_outcome_unknown || task.model_outcome_unknown {
                    "Unknown"
                } else {
                    status(task.status).0
                };
                ListItem::new(vec![
                    Line::styled(flag, self.palette.status(status(task.status).1)),
                    Line::raw(clean(&task.input_preview).replace('\n', " ")),
                    Line::raw(""),
                ])
            })
            .collect();
        if list.is_empty() {
            frame.render_widget(
                Paragraph::new(if self.online && self.snapshot.is_none() && self.busy {
                    "Loading session"
                } else if self.online {
                    "No tasks"
                } else {
                    "Disconnected"
                })
                .block(
                    Block::default()
                        .title(Span::styled(format!(" {title} "), self.palette.label))
                        .style(self.palette.rail),
                ),
                area,
            );
        } else {
            frame.render_stateful_widget(
                List::new(list)
                    .block(
                        Block::default()
                            .title(Span::styled(
                                format!(" {title}  ·  {count} approvals "),
                                self.palette.label,
                            ))
                            .style(self.palette.rail),
                    )
                    .highlight_symbol("› ")
                    .highlight_style(self.palette.choice),
                area,
                &mut state,
            );
        }
    }

    fn draw_detail(&self, frame: &mut Frame<'_>, area: Rect) {
        let mut lines = vec![
            Line::styled(" Task detail", self.palette.label),
            Line::raw(""),
        ];
        if let Some(detail) = &self.detail {
            let task = &detail.task;
            let (label, color) = status(task.status);
            lines.push(Line::styled(label, self.palette.status(color)));
            if let Some(kind) = self
                .snapshot
                .as_ref()
                .and_then(|snap| snap.next_actions.as_ref())
                .and_then(|actions| actions.iter().find(|action| action.task_id == task.id))
                .map(|action| &action.kind)
            {
                let action = match kind {
                    WorkActionKind::ReviewApproval => "Review operation",
                    WorkActionKind::AnswerInput => "Clarification requested",
                    WorkActionKind::Recover { .. } => "Recovery available",
                    WorkActionKind::Reconcile => "External result needs verification",
                    WorkActionKind::ReviewBudget { .. } => "Context budget reached",
                };
                lines.push(Line::styled(action, self.palette.status(Color::Yellow)));
            }
            if let Some(resume) = &detail.resume {
                lines.push(Line::raw(format!("Recovery: {:?}", resume.disposition)));
            }
            if let Some(approval) = &task.pending_approval {
                lines.push(Line::raw(format!(
                    "Approval: {}",
                    clean(&approval.call.name)
                )));
            }
            if let Some(question) = task
                .pending_tool_input
                .as_ref()
                .and_then(|pending| pending.requests["question"].as_str())
            {
                append(&mut lines, question);
            }
            if let Some(plan) = &detail.plan {
                lines.push(Line::styled("Plan", self.palette.label));
                for step in &plan.steps {
                    lines.push(Line::raw(format!(
                        "{:?}  {}",
                        step.status,
                        clean(&step.text)
                    )));
                }
            }
            if !detail.artifacts.is_empty() {
                lines.push(Line::styled("Artifacts", self.palette.label));
                for artifact in &detail.artifacts {
                    lines.push(Line::raw(format!(
                        "{}  |  {} B",
                        clean(&artifact.title),
                        artifact.content_bytes
                    )));
                }
            }
            for message in task
                .messages
                .iter()
                .filter(|message| {
                    matches!(message, Message::User { .. } | Message::Assistant { .. })
                })
                .take(16)
            {
                if let Message::User { text } = message {
                    lines.push(Line::raw(""));
                    lines.push(Line::styled("You", self.palette.label));
                    append(&mut lines, text);
                } else if let Message::Assistant { text } = message
                    && task.output.as_ref() != Some(text)
                {
                    lines.push(Line::raw(""));
                    lines.push(Line::styled("Fliuno", self.palette.label));
                    append(&mut lines, text);
                }
            }
            if let Some(output) = &task.output {
                lines.push(Line::raw(""));
                let label = if task.status == TaskStatus::Failed {
                    "Failure reason"
                } else {
                    "Result"
                };
                lines.push(Line::styled(label, self.palette.label));
                append(&mut lines, output);
            }
        } else {
            lines.push(Line::raw("No task selected"));
        }
        frame.render_widget(
            Paragraph::new(lines)
                .wrap(Wrap { trim: false })
                .scroll((self.scroll, 0)),
            area,
        );
    }

    fn draw_layer(&self, frame: &mut Frame<'_>, area: Rect) {
        if matches!(self.layer, Layer::None | Layer::Edit(Edit::Task)) {
            return;
        }
        let modal = area.inner(Margin::new(
            if area.width < 60 { 1 } else { area.width / 12 },
            if area.height < 12 { 0 } else { area.height / 9 },
        ));
        frame.render_widget(Clear, modal);
        let (title, actions) = match &self.layer {
            Layer::None => return,
            Layer::Commands(_, query) => (format!("Commands  /  {query}"), ""),
            Layer::Workspaces(..) => ("Workspaces".into(), "Open folder"),
            Layer::Sessions(..) => ("Sessions".into(), "New session"),
            Layer::Artifacts(_) => ("Artifacts".into(), ""),
            Layer::Approval { .. } => ("Review operation".into(), "Allow once / Deny"),
            Layer::ConfirmCancel(_) => ("Cancel task?".into(), "Cancel task / Keep running"),
            Layer::Preview(preview, _) => (
                "Artifact preview".into(),
                match (preview.text.is_some(), preview.revision.is_some()) {
                    (true, true) => "Save / Use as input / Previous",
                    (true, false) => "Save / Use as input",
                    (false, true) => "Save / Previous",
                    (false, false) => "Save",
                },
            ),
            Layer::Edit(edit) => (edit.name().into(), ""),
        };
        let block = Block::default()
            .style(self.palette.rail)
            .border_style(self.palette.quiet)
            .borders(Borders::ALL)
            .title(Span::styled(format!(" {title} "), self.palette.label));
        let body = block.inner(modal).inner(Margin::new(1, 0));
        frame.render_widget(block, modal);
        if body.is_empty() {
            return;
        }
        let [body, footer] = Layout::vertical([
            Constraint::Min(0),
            Constraint::Length(u16::from(!actions.is_empty() && body.height > 1)),
        ])
        .areas(body);
        frame.render_widget(Paragraph::new(actions).style(self.palette.label), footer);
        match &self.layer {
            Layer::None => {}
            Layer::Commands(index, query) => render_list(
                frame,
                body,
                self.filtered_commands(query)
                    .into_iter()
                    .map(|(label, _)| label.to_owned())
                    .collect(),
                *index,
                self.palette.choice,
            ),
            Layer::Workspaces(page, index) => render_list(
                frame,
                body,
                page.as_ref()
                    .map(|p| p.items.iter().map(|item| clean(&item.root)).collect())
                    .unwrap_or_default(),
                *index,
                self.palette.choice,
            ),
            Layer::Sessions(page, index) => render_list(
                frame,
                body,
                page.as_ref()
                    .map(|p| p.items.iter().map(session_name).collect())
                    .unwrap_or_default(),
                *index,
                self.palette.choice,
            ),
            Layer::Artifacts(index) => render_list(
                frame,
                body,
                self.detail
                    .as_ref()
                    .map(|d| {
                        d.artifacts
                            .iter()
                            .map(|a| {
                                format!(
                                    "{}  |  {}  |  {} B",
                                    clean(&a.title),
                                    clean(&a.media_type),
                                    a.content_bytes
                                )
                            })
                            .collect()
                    })
                    .unwrap_or_default(),
                *index,
                self.palette.choice,
            ),
            Layer::Approval { text, scroll, .. } => render_text(frame, body, text, *scroll),
            Layer::ConfirmCancel(_) => {}
            Layer::Preview(preview, scroll) => {
                let revision = preview
                    .revision
                    .as_ref()
                    .map(|r| format!("\nPrevious revision: {}", r.previous.artifact_id))
                    .unwrap_or_default();
                let text = format!(
                    "{} bytes\nSHA-256: {}{revision}\n\n{}",
                    preview.digest.content_bytes,
                    preview.digest.sha256,
                    preview.text.as_deref().unwrap_or("No text preview")
                );
                render_text(frame, body, &text, *scroll);
            }
            Layer::Edit(edit) => {
                let input = if let Edit::Answer(_, _, question) = edit {
                    let [prompt, input] =
                        Layout::vertical([Constraint::Length(body.height / 2), Constraint::Min(0)])
                            .areas(body);
                    frame.render_widget(
                        Paragraph::new(clean(question))
                            .wrap(Wrap { trim: false })
                            .style(self.palette.quiet),
                        prompt,
                    );
                    input
                } else {
                    body
                };
                if !input.is_empty() {
                    let (lines, x, y) = self.input.viewport(input.width, input.height);
                    frame.render_widget(
                        Paragraph::new(lines.into_iter().map(Line::raw).collect::<Vec<_>>()),
                        input,
                    );
                    frame.set_cursor_position(Position::new(input.x + x, input.y + y));
                }
            }
        }
    }
}

fn artifact_id(value: &ArtifactSummary) -> ArtifactGetParams {
    ArtifactGetParams {
        task_id: value.task_id.clone(),
        artifact_id: value.id.clone(),
    }
}

fn sessions(workspace: String, after: u64, browse: bool) -> Request {
    Request::Sessions {
        workspace,
        after,
        browse,
    }
}

fn edit_limit(edit: &Edit) -> usize {
    match edit {
        Edit::Task => DRAFT_LIMIT,
        Edit::Session => 120,
        _ => 4096,
    }
}
fn push_bounded(target: &mut String, value: &str, limit: usize) {
    let remaining = limit.saturating_sub(target.len());
    let mut length = value.len().min(remaining);
    while !value.is_char_boundary(length) {
        length -= 1;
    }
    target.push_str(&value[..length]);
}

pub fn clean(value: &str) -> String {
    value
        .chars()
        .filter_map(|c| match c {
            '\n' => Some(c),
            '\t' => Some(' '),
            c if c.is_control()
                || matches!(c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}') =>
            {
                None
            }
            c => Some(c),
        })
        .collect()
}

fn step(key: KeyCode, index: &mut usize, len: usize) {
    match key {
        KeyCode::Up => *index = index.saturating_sub(1),
        KeyCode::Down => *index = (*index + 1).min(len.saturating_sub(1)),
        _ => {}
    }
}

fn scroll_key(key: KeyCode, scroll: &mut u16) {
    match key {
        KeyCode::Up => *scroll = scroll.saturating_sub(1),
        KeyCode::Down => *scroll = scroll.saturating_add(1),
        KeyCode::PageUp => *scroll = scroll.saturating_sub(10),
        KeyCode::PageDown => *scroll = scroll.saturating_add(10),
        _ => {}
    }
}

fn session_name(session: &Session) -> String {
    clean(session.title.as_deref().unwrap_or("Untitled session"))
}

fn status(status: TaskStatus) -> (&'static str, Color) {
    match status {
        TaskStatus::Running => ("Running", ACCENT),
        TaskStatus::WaitingApproval => ("Approval", Color::Yellow),
        TaskStatus::WaitingInput => ("Waiting for input", Color::Yellow),
        TaskStatus::Interrupted => ("Interrupted", Color::Yellow),
        TaskStatus::Completed => ("Completed", Color::Green),
        TaskStatus::Cancelled => ("Cancelled", Color::Gray),
        TaskStatus::Failed => ("Failed", Color::Red),
    }
}

fn render_list(frame: &mut Frame<'_>, area: Rect, items: Vec<String>, index: usize, choice: Style) {
    if items.is_empty() {
        render_text(frame, area, "No entries", 0);
        return;
    }
    let mut state = ListState::default().with_selected(Some(index));
    frame.render_stateful_widget(
        List::new(items)
            .highlight_symbol("> ")
            .highlight_style(choice),
        area,
        &mut state,
    );
}

fn render_text(frame: &mut Frame<'_>, area: Rect, text: &str, scroll: u16) {
    frame.render_widget(
        Paragraph::new(clean(text))
            .wrap(Wrap { trim: false })
            .scroll((scroll, 0)),
        area,
    );
}

fn append(lines: &mut Vec<Line<'static>>, value: &str) {
    lines.extend(
        clean(value)
            .lines()
            .take(500)
            .map(|line| Line::raw(line.to_owned())),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend};

    #[test]
    fn stale_browse_cannot_change_the_current_session() {
        let mut app = App::new(true, Theme::Auto);
        app.workspace = Some(Workspace {
            id: "root".into(),
            root: "test".into(),
        });
        let old = Session {
            id: "old".into(),
            workspace_id: "root".into(),
            title: None,
            archived: false,
        };
        app.session = Some(old.clone());
        app.update(Update::Sessions {
            workspace: "root".into(),
            browse: true,
            page: ObjectPage {
                items: vec![Session {
                    id: "new".into(),
                    ..old
                }],
                next_after: 1,
                has_more: false,
            },
        });
        assert_eq!(app.session.unwrap().id, "old");
        assert!(app.next.is_none());
    }

    #[test]
    fn tiny_layout_draws_and_paste_filters_controls_without_splitting_utf8() {
        let mut app = App::new(true, Theme::Auto);
        app.layer = Layer::Edit(Edit::Task);
        app.paste("交付\x1b\u{202e}\0\t明细\n报告");
        assert_eq!(app.draft.text(), "交付 明细\n报告");
        let mut text = String::new();
        push_bounded(&mut text, app.draft.text(), 4);
        assert_eq!(text, "交");
        for (width, height) in [(1, 1), (20, 5), (48, 18), (80, 24), (120, 32)] {
            let mut screen = Terminal::new(TestBackend::new(width, height)).unwrap();
            screen.draw(|frame| app.draw(frame)).unwrap();
        }
    }

    #[test]
    fn approval_actions_keep_the_terminal_palette_at_supported_sizes() {
        for theme in [Theme::Auto, Theme::Dark, Theme::Light, Theme::Mono] {
            let mut app = App::new(true, theme);
            app.layer = Layer::Approval {
                task: "task".into(),
                id: "approval".into(),
                text: "Operation with many arguments\n".repeat(40),
                scroll: 10,
            };
            for (width, height) in [(48, 18), (80, 24), (120, 32)] {
                let mut screen = Terminal::new(TestBackend::new(width, height)).unwrap();
                let frame = screen.draw(|frame| app.draw(frame)).unwrap();
                let footer = frame
                    .buffer
                    .content()
                    .chunks_exact(width.into())
                    .find(|row| {
                        row.iter()
                            .map(|cell| cell.symbol())
                            .collect::<String>()
                            .contains("Allow once / Deny")
                    })
                    .expect("approval actions stay visible");
                let label = footer.iter().find(|cell| cell.symbol() == "A").unwrap();
                assert_eq!(label.fg, app.palette.label.fg.unwrap_or(Color::Reset));
                assert_eq!(label.bg, app.palette.rail.bg.unwrap_or(Color::Reset));
            }
        }
    }

    #[test]
    fn draft_and_attachment_return_only_in_the_original_session() {
        let mut app = App::new(true, Theme::Mono);
        let session = |id: &str| Session {
            id: id.into(),
            workspace_id: "root".into(),
            title: None,
            archived: false,
        };
        app.select_session(session("one"));
        app.layer = Layer::Edit(Edit::Task);
        app.paste("draft");
        let key = app.key.clone();
        app.input_artifact = Some(ArtifactGetParams {
            task_id: "task".into(),
            artifact_id: "artifact".into(),
        });
        app.select_session(session("two"));
        assert_eq!(app.draft.text(), "");
        assert!(app.input_artifact.is_none());
        app.select_session(session("one"));
        assert_eq!(app.draft.text(), "draft");
        assert_eq!(app.key, key);
        assert_eq!(app.input_artifact.unwrap().artifact_id, "artifact");
    }
}
