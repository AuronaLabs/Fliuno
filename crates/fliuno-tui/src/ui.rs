use crate::service::{Detail, Preview, Request, Update, Worker};
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use fliuno_protocol::{
    ApprovalDecision, ApprovalDetails, ArtifactGetParams, ArtifactSummary, ObjectPage, Session,
    TaskResumeDisposition, TaskStatus, WorkActionKind, WorkSnapshot, Workspace,
};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap},
};
use unicode_segmentation::UnicodeSegmentation;

const ACCENT: Color = Color::Cyan;
const DRAFT_LIMIT: usize = 48 * 1024;

enum Layer {
    Commands(usize),
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
    draft: String,
    key: String,
    input: String,
    input_artifact: Option<ArtifactGetParams>,
    pending_preview: Option<ArtifactGetParams>,
    start_pending: bool,
    start_acknowledged: bool,
    message: String,
}

impl App {
    pub fn new(demo: bool) -> Self {
        Self {
            demo,
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
            draft: String::new(),
            key: uuid::Uuid::new_v4().to_string(),
            input: String::new(),
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
        self.next = Some(Request::SelectSession(session.id.clone()));
        self.session = Some(session);
        self.snapshot = None;
        self.selected = None;
        self.detail = None;
        self.input_artifact = None;
        self.scroll = 0;
        self.layer = Layer::None;
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
            let length = target.len();
            push_bounded(target, &text, edit_limit(edit));
            if target.len() != length && matches!(edit, Edit::Task) {
                self.key = uuid::Uuid::new_v4().to_string();
            }
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
            match key.code {
                KeyCode::Char('s') if pressed && ctrl => {
                    self.submit(edit, worker);
                }
                KeyCode::Enter if pressed && !matches!(edit, Edit::Task) => {
                    self.submit(edit, worker);
                }
                KeyCode::Enter if matches!(edit, Edit::Task) => {
                    push_bounded(value, "\n", edit_limit(&edit));
                    self.key = uuid::Uuid::new_v4().to_string();
                }
                KeyCode::Backspace => {
                    if let Some((offset, _)) = value.grapheme_indices(true).next_back() {
                        value.truncate(offset);
                    }
                    if matches!(edit, Edit::Task) {
                        self.key = uuid::Uuid::new_v4().to_string();
                    }
                }
                KeyCode::Char(c)
                    if !ctrl && !key.modifiers.contains(KeyModifiers::ALT) && !c.is_control() =>
                {
                    push_bounded(value, c.encode_utf8(&mut [0; 4]), edit_limit(&edit));
                    if matches!(edit, Edit::Task) {
                        self.key = uuid::Uuid::new_v4().to_string();
                    }
                }
                _ => {}
            }
            return false;
        }

        let mut request = None;
        let mut close = false;
        if let Layer::Commands(index) = self.layer {
            let commands = self.commands();
            let mut index = index.min(commands.len().saturating_sub(1));
            step(key.code, &mut index, commands.len());
            if pressed && key.code == KeyCode::Enter {
                let (_, command) = commands[index];
                self.layer = Layer::None;
                return self.overview_key(KeyEvent::new(command, KeyModifiers::NONE), worker);
            }
            self.layer = Layer::Commands(index);
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
        if key.code == KeyCode::F(1) && pressed {
            self.layer = Layer::Commands(0);
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
            self.draft.trim()
        } else {
            self.input.trim()
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
        let [top, middle, bottom] = Layout::vertical([
            Constraint::Length(2),
            Constraint::Min(0),
            Constraint::Length(1),
        ])
        .areas(area);
        let root = self
            .workspace
            .as_ref()
            .map(|root| clean(&root.root))
            .unwrap_or_default();
        let session = self.session.as_ref().map(session_name).unwrap_or_default();
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
                    Span::styled(
                        " Fliuno ",
                        Style::default()
                            .fg(Color::Black)
                            .bg(ACCENT)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::raw(format!("  {state}   {session}")),
                ]),
                Line::styled(format!(" {root}"), Style::default().fg(Color::Gray)),
            ]),
            top,
        );
        if area.width >= 84 {
            let [tasks, details] =
                Layout::horizontal([Constraint::Percentage(34), Constraint::Percentage(66)])
                    .areas(middle);
            self.draw_tasks(frame, tasks);
            self.draw_detail(frame, details);
        } else if self.focus_details {
            self.draw_detail(frame, middle);
        } else {
            self.draw_tasks(frame, middle);
        }
        let message = if self.busy { "Working" } else { &self.message };
        frame.render_widget(
            Paragraph::new(clean(message)).style(Style::default().fg(if self.online {
                Color::Gray
            } else {
                Color::Yellow
            })),
            bottom,
        );
        self.draw_layer(frame, area);
    }

    fn draw_tasks(&self, frame: &mut Frame<'_>, area: Rect) {
        let tasks = self.tasks();
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
                    Line::styled(flag, Style::default().fg(status(task.status).1)),
                    Line::raw(clean(&task.input_preview).replace('\n', " ")),
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
                .block(panel("Tasks", !self.focus_details)),
                area,
            );
        } else {
            frame.render_stateful_widget(
                List::new(list)
                    .block(panel(
                        format!("Tasks  |  {count} approvals"),
                        !self.focus_details,
                    ))
                    .highlight_symbol("> ")
                    .highlight_style(Style::default().bg(Color::DarkGray).fg(Color::White)),
                area,
                &mut state,
            );
        }
    }

    fn draw_detail(&self, frame: &mut Frame<'_>, area: Rect) {
        let mut lines = Vec::new();
        if let Some(detail) = &self.detail {
            let task = &detail.task;
            let (label, color) = status(task.status);
            lines.push(Line::styled(
                label,
                Style::default().fg(color).add_modifier(Modifier::BOLD),
            ));
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
                lines.push(Line::styled(action, Style::default().fg(Color::Yellow)));
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
                lines.push(Line::styled("Plan", Style::default().fg(ACCENT)));
                for step in &plan.steps {
                    lines.push(Line::raw(format!(
                        "{:?}  {}",
                        step.status,
                        clean(&step.text)
                    )));
                }
            }
            if let Some(output) = &task.output {
                lines.push(Line::raw(""));
                append(&mut lines, output);
            }
            if !detail.artifacts.is_empty() {
                lines.push(Line::styled("Artifacts", Style::default().fg(ACCENT)));
                for artifact in &detail.artifacts {
                    lines.push(Line::raw(format!(
                        "{}  |  {} B",
                        clean(&artifact.title),
                        artifact.content_bytes
                    )));
                }
            }
        } else {
            lines.push(Line::raw("No task selected"));
        }
        frame.render_widget(
            Paragraph::new(lines)
                .block(panel("Task detail", self.focus_details))
                .wrap(Wrap { trim: false })
                .scroll((self.scroll, 0)),
            area,
        );
    }

    fn draw_layer(&self, frame: &mut Frame<'_>, area: Rect) {
        if matches!(self.layer, Layer::None) {
            return;
        }
        let [_, modal, _] = Layout::horizontal([
            Constraint::Percentage(8),
            Constraint::Percentage(84),
            Constraint::Percentage(8),
        ])
        .areas(area);
        let [_, modal, _] = Layout::vertical([
            Constraint::Percentage(15),
            Constraint::Percentage(70),
            Constraint::Percentage(15),
        ])
        .areas(modal);
        frame.render_widget(Clear, modal);
        match &self.layer {
            Layer::None => {}
            Layer::Commands(index) => render_list(
                frame,
                modal,
                "Commands",
                self.commands()
                    .into_iter()
                    .map(|(label, _)| label.to_owned())
                    .collect(),
                *index,
            ),
            Layer::Workspaces(page, index) => render_list(
                frame,
                modal,
                "Workspaces  |  Open folder",
                page.as_ref()
                    .map(|p| p.items.iter().map(|item| clean(&item.root)).collect())
                    .unwrap_or_default(),
                *index,
            ),
            Layer::Sessions(page, index) => render_list(
                frame,
                modal,
                "Sessions  |  New session",
                page.as_ref()
                    .map(|p| p.items.iter().map(session_name).collect())
                    .unwrap_or_default(),
                *index,
            ),
            Layer::Artifacts(index) => render_list(
                frame,
                modal,
                "Artifacts",
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
            ),
            Layer::Approval { text, scroll, .. } => render_text(
                frame,
                modal,
                "Review operation  |  Allow once / Deny",
                text,
                *scroll,
            ),
            Layer::ConfirmCancel(_) => render_text(
                frame,
                modal,
                "Cancel task?",
                "Cancel task  /  Keep running",
                0,
            ),
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
                render_text(
                    frame,
                    modal,
                    "Artifact preview  |  Save / Use as input",
                    &text,
                    *scroll,
                );
            }
            Layer::Edit(edit) => {
                let value = if matches!(edit, Edit::Task) {
                    &self.draft
                } else {
                    &self.input
                };
                let caption = match edit {
                    Edit::Answer(_, _, question) => format!("{}\n\n", clean(question)),
                    Edit::Task if self.input_artifact.is_some() => {
                        "Previous artifact attached\n\n".into()
                    }
                    _ => String::new(),
                };
                let content = format!("{caption}{value}");
                let scroll = content
                    .lines()
                    .count()
                    .saturating_sub(modal.height.saturating_sub(3) as usize)
                    .min(u16::MAX as usize) as u16;
                render_text(frame, modal, edit.name(), &content, scroll);
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

fn panel<'a>(title: impl Into<Line<'a>>, active: bool) -> Block<'a> {
    Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(if active { ACCENT } else { Color::DarkGray }))
}

fn render_list(frame: &mut Frame<'_>, area: Rect, title: &str, items: Vec<String>, index: usize) {
    if items.is_empty() {
        render_text(frame, area, title, "No entries", 0);
        return;
    }
    let mut state = ListState::default().with_selected(Some(index));
    frame.render_stateful_widget(
        List::new(items)
            .block(panel(title, true))
            .highlight_symbol("> ")
            .highlight_style(Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)),
        area,
        &mut state,
    );
}

fn render_text(frame: &mut Frame<'_>, area: Rect, title: &str, text: &str, scroll: u16) {
    frame.render_widget(
        Paragraph::new(clean(text))
            .block(panel(title, true))
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
        let mut app = App::new(true);
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
        let mut app = App::new(true);
        app.layer = Layer::Edit(Edit::Task);
        app.paste("交付\x1b\u{202e}\0\t明细\n报告");
        assert_eq!(app.draft, "交付 明细\n报告");
        let mut text = String::new();
        push_bounded(&mut text, &app.draft, 4);
        assert_eq!(text, "交");
        for (width, height) in [(20, 5), (48, 18), (110, 28)] {
            let mut screen = Terminal::new(TestBackend::new(width, height)).unwrap();
            screen.draw(|frame| app.draw(frame)).unwrap();
        }
    }
}
