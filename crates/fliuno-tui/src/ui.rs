use crate::{
    editor::Editor,
    options::Theme,
    service::{Detail, HistoryPage, Preview, Request, Update, Worker, WorkspaceInbox},
    task_browser::{PageAction, TaskBrowser},
};
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use fliuno_protocol::{
    ApprovalDecision, ApprovalDetails, ApprovalSummary, ArtifactGetParams, ArtifactSummary,
    Message, ObjectPage, Session, SessionHistoryRole, Task, TaskResumeDisposition, TaskStatus,
    TaskSummary, WorkActionKind, WorkSnapshot, Workspace,
};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Margin, Position, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap},
};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

const ACCENT: Color = Color::Cyan;
use fliuno_protocol::{
    ContextDetail, ContextMessage, ContextMode, ContextOptions, EventKind, SessionContextParams,
    SessionForkParams, SessionListOrder, SessionListParams, TaskContext, TaskContextAttribution,
    TaskQueue, TaskQueueItemParams, TaskQueueState, TaskTimelinePage, TaskTimelineParams,
};

const DRAFT_LIMIT: usize = 48 * 1024;

struct Palette {
    base: Style,
    rail: Style,
    label: Style,
    quiet: Style,
    choice: Style,
    status_colors: Option<[Color; 3]>,
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
                    status_colors: Some(if matches!(theme, Theme::Dark) {
                        [
                            rgb((247, 206, 121)),
                            rgb((244, 145, 145)),
                            rgb((138, 225, 182)),
                        ]
                    } else {
                        [rgb((124, 76, 16)), rgb((153, 46, 47)), rgb((17, 100, 68))]
                    }),
                    mono: false,
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
                    status_colors: None,
                    mono,
                }
            }
        }
    }

    fn status(&self, color: Color) -> Style {
        if self.mono {
            self.label
        } else if let Some([warning, error, success]) = self.status_colors {
            match color {
                Color::Yellow => self.label.fg(warning),
                Color::Red => self.label.fg(error),
                Color::Green => self.label.fg(success),
                Color::Gray => self.quiet,
                _ => self.label,
            }
        } else {
            self.label.fg(color)
        }
    }
}

enum Layer {
    Commands(usize, String),
    Inbox(usize),
    History(usize),
    TaskBrowser(TaskBrowser),
    Workspaces(Option<ObjectPage<Workspace>>, usize),
    Sessions(Option<ObjectPage<Session>>, usize),
    Artifacts(usize),
    Queue(Option<TaskQueue>, usize),
    Timeline(Option<TaskTimelinePage>, usize),
    TimelineEvent {
        page: TaskTimelinePage,
        index: usize,
        sequence: u64,
        text: String,
        scroll: u16,
    },
    Context(String, u16),
    DraftContext {
        preview: Option<Box<TaskContext>>,
        scroll: u16,
    },
    Approval {
        task: String,
        id: String,
        text: String,
        allow_once: bool,
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
    SessionSearch,
    RenameSession(Session),
    Fork(SessionForkParams),
    Answer(String, String, String),
    Steer(String),
    HistorySearch,
    Download(ArtifactGetParams),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum InboxFilter {
    Attention,
    Active,
    Recent,
}

#[derive(Clone)]
enum InboxRow {
    Approval {
        task_id: String,
        session_id: String,
        approval_id: String,
    },
    Task(TaskSummary),
}

impl InboxRow {
    fn key(&self) -> String {
        match self {
            Self::Approval {
                task_id,
                approval_id,
                ..
            } => format!("approval:{task_id}:{approval_id}"),
            Self::Task(task) => format!("task:{}", task.id),
        }
    }
}

impl Edit {
    fn name(&self) -> &str {
        match self {
            Self::Task => "New task",
            Self::Workspace => "Open folder",
            Self::Session => "New session",
            Self::SessionSearch => "Find sessions by title or ID",
            Self::RenameSession(_) => "Rename session",
            Self::Fork(_) => "Name the session branch",
            Self::Answer(_, _, _) => "Answer clarification",
            Self::Steer(_) => "Add instruction",
            Self::HistorySearch => "Search workspace history",
            Self::Download(_) => "Save artifact to",
        }
    }
}

struct ParkedDraft {
    session_id: String,
    text: Editor,
    key: String,
    artifact: Option<ArtifactGetParams>,
    context: ContextOptions,
    plan_only: bool,
}

pub struct App {
    demo: bool,
    palette: Palette,
    online: bool,
    busy: bool,
    next: Option<Request>,
    workspace: Option<Workspace>,
    session: Option<Session>,
    session_query: Option<String>,
    session_archived: bool,
    snapshot: Option<WorkSnapshot>,
    inbox: Option<WorkspaceInbox>,
    inbox_filter: InboxFilter,
    inbox_selection: Option<String>,
    inbox_approvals: Vec<ApprovalSummary>,
    inbox_approvals_more: bool,
    history_page: Option<HistoryPage>,
    history_query: String,
    history_role: Option<SessionHistoryRole>,
    history_selection: Option<String>,
    selected: Option<String>,
    detail: Option<Box<Detail>>,
    scroll: u16,
    focus_details: bool,
    layer: Layer,
    draft: Editor,
    parked: Vec<ParkedDraft>,
    draft_context: ContextOptions,
    plan_only: bool,
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
            session_query: None,
            session_archived: false,
            snapshot: None,
            inbox: None,
            inbox_filter: InboxFilter::Attention,
            inbox_selection: None,
            inbox_approvals: Vec::new(),
            inbox_approvals_more: false,
            history_page: None,
            history_query: String::new(),
            history_role: None,
            history_selection: None,
            selected: None,
            detail: None,
            scroll: 0,
            focus_details: false,
            layer: Layer::None,
            draft: Editor::default(),
            parked: Vec::new(),
            draft_context: ContextOptions::default(),
            plan_only: false,
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

    fn install_session(&mut self, session: Session) {
        if self.session.as_ref().map(|current| &current.id) != Some(&session.id) {
            self.park_draft();
            if let Some(i) = self
                .parked
                .iter()
                .position(|draft| draft.session_id == session.id)
            {
                let draft = self.parked.remove(i);
                self.draft = draft.text;
                self.key = draft.key;
                self.input_artifact = draft.artifact;
                self.plan_only = draft.plan_only;
                self.draft_context = draft.context;
            }
        }
        self.session = Some(session);
        self.next = None;
        self.snapshot = None;
        self.selected = None;
        self.detail = None;
        self.scroll = 0;
        self.layer = Layer::None;
    }

    fn select_session(&mut self, session: Session) {
        let id = session.id.clone();
        self.install_session(session);
        self.next = Some(Request::SelectSession(id));
    }

    fn inbox_rows(&self) -> Vec<InboxRow> {
        let Some(inbox) = &self.inbox else {
            return Vec::new();
        };
        let snapshot = &inbox.snapshot;
        let tasks = snapshot.recent_tasks.as_deref().unwrap_or_default();
        match self.inbox_filter {
            InboxFilter::Attention => {
                let mut rows = Vec::new();
                for approval in &self.inbox_approvals {
                    rows.push(InboxRow::Approval {
                        task_id: approval.task_id.clone(),
                        session_id: approval.session_id.clone(),
                        approval_id: approval.id.clone(),
                    });
                }
                for task in tasks {
                    let listed = self
                        .inbox_approvals
                        .iter()
                        .filter(|approval| approval.task_id == task.id)
                        .count() as u32;
                    let actions = snapshot.next_actions.as_deref().unwrap_or_default();
                    let action = actions.iter().find(|action| action.task_id == task.id);
                    let needs_action = task.pending_approval_count > listed
                        || matches!(
                            task.status,
                            TaskStatus::WaitingInput | TaskStatus::Interrupted | TaskStatus::Failed
                        )
                        || task.model_outcome_unknown
                        || task.agent_outcome_unknown
                        || action.is_some_and(|action| {
                            !matches!(action.kind, WorkActionKind::ReviewApproval)
                        });
                    if needs_action {
                        rows.push(InboxRow::Task(task.clone()));
                    }
                }
                rows
            }
            InboxFilter::Active => tasks
                .iter()
                .filter(|task| task.status == TaskStatus::Running)
                .cloned()
                .map(InboxRow::Task)
                .collect(),
            InboxFilter::Recent => tasks
                .iter()
                .filter(|task| task.status.is_terminal())
                .cloned()
                .map(InboxRow::Task)
                .collect(),
        }
    }

    fn update_inbox(&mut self, inbox: WorkspaceInbox) {
        self.inbox_approvals = inbox.snapshot.approvals.page.items.clone();
        self.inbox_approvals_more = inbox.snapshot.approvals.page.has_more;
        self.inbox = Some(inbox);
        if matches!(self.layer, Layer::Inbox(_)) {
            let rows = self.inbox_rows();
            let selected = self
                .inbox_selection
                .as_deref()
                .and_then(|key| rows.iter().position(|row| row.key() == key))
                .unwrap_or(0);
            self.inbox_selection = rows.get(selected).map(InboxRow::key);
            self.layer = Layer::Inbox(selected);
        }
    }

    fn park_draft(&mut self) {
        let draft = std::mem::take(&mut self.draft);
        let artifact = self.input_artifact.take();
        let plan_only = std::mem::take(&mut self.plan_only);
        let context = std::mem::take(&mut self.draft_context);
        let key = std::mem::replace(&mut self.key, uuid::Uuid::new_v4().to_string());
        if let Some(session) = &self.session
            && (!draft.text().is_empty()
                || artifact.is_some()
                || plan_only
                || context != ContextOptions::default())
        {
            if self.parked.len() >= 8 {
                self.parked.remove(0);
            }
            self.parked.push(ParkedDraft {
                session_id: session.id.clone(),
                text: draft,
                key,
                artifact,
                context,
                plan_only,
            });
        }
    }

    fn toggle_plan_mode(&mut self) {
        self.plan_only = !self.plan_only;
        self.key = uuid::Uuid::new_v4().to_string();
        self.message = if self.plan_only {
            "Plan mode enabled"
        } else {
            "Execution mode enabled"
        }
        .into();
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
                self.inbox = None;
                self.inbox_approvals.clear();
                self.inbox_approvals_more = false;
                self.history_page = None;
                self.session_query = None;
                self.session_archived = false;
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
            Update::SessionMetadata(saved) => {
                if self.session.as_ref().is_some_and(|s| s.id == saved.id) {
                    self.session = Some(saved);
                }
                self.message = "Session updated".into();
            }
            Update::Inbox(inbox) => self.update_inbox(inbox),
            Update::Approvals(approvals, has_more) => {
                self.inbox_approvals.extend(approvals);
                self.inbox_approvals_more = has_more;
                if matches!(self.layer, Layer::Inbox(_)) {
                    let rows = self.inbox_rows();
                    let selected = self
                        .inbox_selection
                        .as_deref()
                        .and_then(|key| rows.iter().position(|row| row.key() == key))
                        .unwrap_or(0);
                    self.inbox_selection = rows.get(selected).map(InboxRow::key);
                    self.layer = Layer::Inbox(selected);
                }
            }
            Update::Jump(session, task) => {
                if self
                    .workspace
                    .as_ref()
                    .is_some_and(|workspace| workspace.id == session.workspace_id)
                {
                    self.install_session(session);
                    self.selected = Some(task);
                    self.focus_details = true;
                }
            }
            Update::History(page, append) => {
                if append {
                    if let Some(current) = &mut self.history_page
                        && current.query == page.query
                        && current.role == page.role
                        && current.through_sequence == page.through_sequence
                    {
                        current.items.extend(page.items);
                        current.next_before_sequence = page.next_before_sequence;
                        current.scanned_turns =
                            current.scanned_turns.saturating_add(page.scanned_turns);
                        current.skipped_oversized_turns = current
                            .skipped_oversized_turns
                            .saturating_add(page.skipped_oversized_turns);
                    }
                } else {
                    self.history_page = Some(page);
                    self.history_selection = None;
                }
                if matches!(self.layer, Layer::History(_)) {
                    let items = self.history_page.as_ref().map(|page| &page.items);
                    let index = self
                        .history_selection
                        .as_deref()
                        .and_then(|id| {
                            items.and_then(|items| items.iter().position(|item| item.task.id == id))
                        })
                        .unwrap_or(0);
                    self.history_selection = items
                        .and_then(|items| items.get(index))
                        .map(|item| item.task.id.clone());
                    self.layer = Layer::History(index);
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
                if matches!(self.layer, Layer::None) {
                    if self.selected.is_none()
                        && let Some(task) = self.tasks().first()
                    {
                        self.select_task(task.id.clone());
                    } else if previous != Some(current)
                        && let Some(id) = &self.selected
                    {
                        self.next = Some(Request::Task(id.clone()));
                    }
                }
            }
            Update::TaskBrowser {
                session,
                after,
                action,
                page,
            } => {
                if let Layer::TaskBrowser(browser) = &mut self.layer
                    && browser.session == session
                {
                    browser.install(action, after, page);
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
                    && let Some((text, allow_once)) = approval_review(
                        &task,
                        &id,
                        self.workspace
                            .as_ref()
                            .map(|root| display_path(&root.root))
                            .as_deref()
                            .unwrap_or(""),
                        *details,
                    )
                {
                    self.message.clear();
                    self.layer = Layer::Approval {
                        task,
                        id,
                        text,
                        allow_once,
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
            Update::Timeline(page) => {
                if self.selected.as_deref() == Some(&page.task_id)
                    && matches!(self.layer, Layer::Timeline(..))
                {
                    self.layer = Layer::Timeline(Some(page), 0);
                }
            }
            Update::Event(sequence, text) => {
                if let Layer::Timeline(Some(page), index) = &self.layer
                    && page
                        .entries
                        .get(*index)
                        .is_some_and(|entry| entry.sequence == sequence)
                {
                    self.layer = Layer::TimelineEvent {
                        page: page.clone(),
                        index: *index,
                        sequence,
                        text,
                        scroll: 0,
                    };
                }
            }
            Update::ContextAttribution(attribution) => {
                if self.selected.as_deref() == Some(&attribution.task_id)
                    && matches!(self.layer, Layer::Context(..))
                {
                    self.layer = Layer::Context(format_context_attribution(&attribution), 0);
                }
            }
            Update::Queue(queue) => {
                if self
                    .session
                    .as_ref()
                    .is_some_and(|s| s.id == queue.session_id)
                    && let Layer::Queue(current, index) = &mut self.layer
                {
                    *index = (*index).min(queue.pending.len().saturating_sub(1));
                    *current = Some(queue);
                }
            }
            Update::DraftContext(session, context) => {
                if self
                    .session
                    .as_ref()
                    .is_some_and(|current| current.id == session)
                    && let Layer::DraftContext { preview, scroll } = &mut self.layer
                {
                    *preview = Some(context);
                    *scroll = 0;
                }
            }
            Update::Enqueued(item) => {
                self.draft_context.through_sequence = None;
                self.draft.clear();
                self.input_artifact = None;
                self.key = uuid::Uuid::new_v4().to_string();
                self.layer = Layer::None;
                self.message = match item.status {
                    TaskQueueState::Pending => "Prompt queued (Q to open)".into(),
                    TaskQueueState::Cancelled => "Prompt was already cancelled".into(),
                    TaskQueueState::Started { .. } => "Prompt already started".into(),
                };
            }
            Update::StartedQueued(id) => {
                self.select_task(id);
                self.next = None;
                self.layer = Layer::None;
                self.message = "Queued prompt started".into();
            }
            Update::Started(id) => {
                self.draft_context.through_sequence = None;
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
        if key.code == KeyCode::Esc && matches!(self.layer, Layer::Inbox(_)) {
            if !self.start_pending {
                self.layer = Layer::None;
                self.next = Some(Request::WorkspaceInbox(false));
                self.sync(worker);
            }
            return false;
        }
        if key.code == KeyCode::Esc
            && let Layer::TimelineEvent { page, index, .. } = &self.layer
        {
            self.layer = Layer::Timeline(Some(page.clone()), *index);
            return false;
        }
        if matches!(self.layer, Layer::DraftContext { .. }) {
            if key.code == KeyCode::Esc || key.code == KeyCode::F(3) {
                self.layer = Layer::Edit(Edit::Task);
            } else {
                self.draft_context_key(key, worker);
            }
            return false;
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
            if matches!(edit, Edit::Task) && pressed && key.code == KeyCode::F(2) && !self.busy {
                self.toggle_plan_mode();
                return false;
            }
            if matches!(edit, Edit::Task) && pressed && key.code == KeyCode::F(3) && !self.busy {
                self.open_draft_context(worker);
                return false;
            }
            let value = if matches!(edit, Edit::Task) {
                &mut self.draft
            } else {
                &mut self.input
            };
            let enqueue = matches!(edit, Edit::Task) && ctrl && key.code == KeyCode::Char('e');
            if pressed
                && (enqueue
                    || (ctrl && key.code == KeyCode::Char('s'))
                    || (key.code == KeyCode::Enter && !matches!(edit, Edit::Task)))
            {
                self.submit(edit, worker, enqueue);
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
        if matches!(self.layer, Layer::Inbox(_)) {
            if self.inbox_key(key, worker) {
                return true;
            }
            self.sync(worker);
            return false;
        }
        if matches!(self.layer, Layer::History(_)) {
            if self.history_key(key, worker) {
                return true;
            }
            self.sync(worker);
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
                if self.busy {
                    return false;
                }
                if pressed && key.code == KeyCode::Char('f') {
                    self.input.clear();
                    if let Some(query) = &self.session_query {
                        let _ = self.input.insert(query, 480);
                    }
                    self.layer = Layer::Edit(Edit::SessionSearch);
                    return false;
                }
                if pressed && key.code == KeyCode::Char('n') {
                    self.input.clear();
                    self.layer = Layer::Edit(Edit::Session);
                    return false;
                }
                if pressed
                    && key.code == KeyCode::Char('A')
                    && let Some(root) = &self.workspace
                {
                    self.session_archived = !self.session_archived;
                    *page = None;
                    *index = 0;
                    request = Some(Request::Sessions(
                        session_catalog(
                            root.id.clone(),
                            0,
                            self.session_archived,
                            self.session_query.clone(),
                        ),
                        true,
                    ));
                }
                if let (Some(page), Some(root)) = (page, &self.workspace) {
                    step(key.code, index, page.items.len());
                    if pressed && let Some(item) = page.items.get(*index) {
                        match key.code {
                            KeyCode::Char('r') => {
                                self.input.clear();
                                if let Some(title) = &item.title {
                                    let _ = self.input.insert(title, 480);
                                }
                                self.layer = Layer::Edit(Edit::RenameSession(item.clone()));
                                return false;
                            }
                            KeyCode::Char('x') => {
                                request = Some(Request::UpdateSession {
                                    session: item.clone(),
                                    title: None,
                                    catalog: session_catalog(
                                        root.id.clone(),
                                        0,
                                        self.session_archived,
                                        self.session_query.clone(),
                                    ),
                                });
                            }
                            _ => {}
                        }
                    }
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
                        request = Some(Request::Sessions(
                            session_catalog(
                                root.id.clone(),
                                page.next_after,
                                self.session_archived,
                                self.session_query.clone(),
                            ),
                            true,
                        ));
                    } else if pressed && key.code == KeyCode::PageUp {
                        request = Some(Request::Sessions(
                            session_catalog(
                                root.id.clone(),
                                0,
                                self.session_archived,
                                self.session_query.clone(),
                            ),
                            true,
                        ));
                    }
                }
            }
            Layer::Timeline(page, index) => {
                if !self.busy
                    && let Some(page) = page
                {
                    step(key.code, index, page.entries.len());
                    let mut timeline = TaskTimelineParams {
                        task_id: page.task_id.clone(),
                        after: 0,
                        through_sequence: Some(page.through_sequence),
                    };
                    if pressed {
                        match key.code {
                            KeyCode::PageDown => {
                                if let Some(after) = page.next_after {
                                    timeline.after = after;
                                    request = Some(Request::Timeline(timeline));
                                }
                            }
                            KeyCode::Home => request = Some(Request::Timeline(timeline)),
                            KeyCode::Char('r') => {
                                timeline.through_sequence = None;
                                request = Some(Request::Timeline(timeline));
                            }
                            KeyCode::Char('a') => {
                                let approval = page
                                    .entries
                                    .get(*index)
                                    .and_then(|entry| entry.approval_id.as_deref());
                                let pending = self
                                    .detail
                                    .as_ref()
                                    .and_then(|detail| pending_approval_id(&detail.task));
                                if let Some(id) = approval.filter(|id| Some(*id) == pending) {
                                    request =
                                        Some(Request::Approval(page.task_id.clone(), id.into()));
                                    close = true;
                                } else {
                                    self.message = "Selected event has no pending approval".into();
                                }
                            }
                            KeyCode::Enter => {
                                if let Some(entry) = page.entries.get(*index) {
                                    if entry.payload_bytes <= 64 * 1024 {
                                        request = Some(Request::Event(
                                            page.task_id.clone(),
                                            entry.sequence,
                                        ));
                                    } else {
                                        self.message = format!(
                                            "Event {} is too large to preview ({} B)",
                                            entry.sequence, entry.payload_bytes
                                        );
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                }
            }
            Layer::DraftContext { .. } => {}
            Layer::Context(_, scroll) | Layer::TimelineEvent { scroll, .. } => {
                scroll_key(key.code, scroll)
            }
            Layer::Queue(queue, index) => {
                if pressed && !self.busy && key.code == KeyCode::Char('r') {
                    request = self.session.as_ref().map(|s| Request::Queue(s.id.clone()));
                }
                if let Some(queue) = queue {
                    if pressed
                        && !self.busy
                        && key.code == KeyCode::Char('g')
                        && let Some(blocker) = queue.blocking_task_id.clone()
                    {
                        self.select_task(blocker);
                        self.layer = Layer::None;
                        self.sync(worker);
                        return false;
                    }
                    step(key.code, index, queue.pending.len());
                    if pressed
                        && !self.busy
                        && let Some(item) = queue.pending.get(*index)
                    {
                        let target = TaskQueueItemParams {
                            session_id: queue.session_id.clone(),
                            queue_id: item.queue_id.clone(),
                        };
                        match key.code {
                            KeyCode::Enter if *index == 0 => {
                                request = Some(Request::StartQueued(target))
                            }
                            KeyCode::Enter => {
                                self.message = "Start the first prompt in the queue".into()
                            }
                            KeyCode::Delete => request = Some(Request::CancelQueued(target)),
                            _ => {}
                        }
                    }
                }
            }
            Layer::TaskBrowser(browser) => {
                if self.busy {
                    return false;
                }
                step(
                    key.code,
                    &mut browser.index,
                    browser.page.as_ref().map_or(0, |p| p.items.len()),
                );
                if pressed {
                    let action = match key.code {
                        KeyCode::PageDown => Some(PageAction::Next),
                        KeyCode::PageUp => Some(PageAction::Previous),
                        KeyCode::Home | KeyCode::Char('r') => Some(PageAction::First),
                        KeyCode::Char('f') => Some(PageAction::CycleFilter),
                        KeyCode::Enter => {
                            if let Some(id) = browser.selected().map(|task| task.id.clone()) {
                                self.select_task(id);
                                self.layer = Layer::None;
                                self.focus_details = true;
                                self.sync(worker);
                                return false;
                            }
                            None
                        }
                        _ => None,
                    };
                    if let Some(action) = action
                        && let Some(params) = browser.query(action)
                    {
                        request = Some(Request::TaskBrowser(params, action));
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
                task,
                id,
                allow_once,
                scroll,
                ..
            } => {
                scroll_key(key.code, scroll);
                if pressed {
                    let decision = match key.code {
                        KeyCode::Char('y') if *allow_once => Some(ApprovalDecision::AllowOnce),
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
            Layer::Inbox(..) | Layer::History(..) | Layer::Commands(..) | Layer::Edit(..) => {
                unreachable!()
            }
        }
        if let Some(request) = request
            && self.send(worker, request)
            && close
        {
            self.layer = Layer::None;
        }
        false
    }

    fn open_draft_context(&mut self, worker: &Worker) {
        if self.session.is_none() {
            return;
        }
        self.layer = Layer::DraftContext {
            preview: None,
            scroll: 0,
        };
        self.fetch_draft_context(worker);
    }

    fn fetch_draft_context(&mut self, worker: &Worker) {
        if let Some(session) = &self.session {
            let params = SessionContextParams {
                session_id: session.id.clone(),
                through_sequence: None,
                options: self.draft_context.clone(),
            };
            self.send(worker, Request::DraftContext(Box::new(params)));
        }
    }

    fn draft_context_key(&mut self, key: KeyEvent, worker: &Worker) {
        let Layer::DraftContext { preview, scroll } = &mut self.layer else {
            return;
        };
        scroll_key(key.code, scroll);
        if self.busy || !self.online || key.kind != KeyEventKind::Press {
            return;
        }
        let options = &mut self.draft_context;
        match key.code {
            KeyCode::Char('s') => {
                options.mode = match options.mode {
                    ContextMode::Session => ContextMode::Isolated,
                    ContextMode::Isolated => ContextMode::Session,
                };
                options.detail = ContextDetail::Full;
                options.through_sequence = None;
            }
            KeyCode::Char('m') if options.mode == ContextMode::Session => {
                options.disable_memory = !options.disable_memory;
            }
            KeyCode::Char('h') if options.mode == ContextMode::Session => {
                options.detail = match options.detail {
                    ContextDetail::Full => ContextDetail::Conversation,
                    ContextDetail::Conversation => ContextDetail::Full,
                };
            }
            KeyCode::Char('p') if options.mode == ContextMode::Session => {
                options.through_sequence = match options.through_sequence {
                    Some(_) => None,
                    None => preview.as_ref().map(|value| value.through_sequence),
                };
            }
            KeyCode::Char('r') => {}
            _ => return,
        }
        self.key = uuid::Uuid::new_v4().to_string();
        *preview = None;
        *scroll = 0;
        self.fetch_draft_context(worker);
    }

    fn inbox_key(&mut self, key: KeyEvent, worker: &Worker) -> bool {
        let pressed = key.kind == KeyEventKind::Press;
        if !pressed {
            return false;
        }
        if matches!(key.code, KeyCode::F(1) | KeyCode::Char('/')) {
            self.layer = Layer::Commands(0, String::new());
            self.next = Some(Request::WorkspaceInbox(false));
            return false;
        }
        if key.code == KeyCode::Char('q') {
            return true;
        }
        if key.code == KeyCode::Char('h') || key.code == KeyCode::Char('H') {
            self.input.clear();
            self.layer = Layer::Edit(Edit::HistorySearch);
            self.next = Some(Request::WorkspaceInbox(false));
            return false;
        }
        if key.code == KeyCode::Char('r') || key.code == KeyCode::Char('R') {
            if !self.busy {
                self.send(worker, Request::WorkspaceInbox(true));
            }
            return false;
        }
        if !self.busy {
            match key.code {
                KeyCode::Char('1') => {
                    self.inbox_filter = InboxFilter::Attention;
                    self.inbox_selection = None;
                    self.layer = Layer::Inbox(0);
                }
                KeyCode::Char('2') => {
                    self.inbox_filter = InboxFilter::Active;
                    self.inbox_selection = None;
                    self.layer = Layer::Inbox(0);
                }
                KeyCode::Char('3') => {
                    self.inbox_filter = InboxFilter::Recent;
                    self.inbox_selection = None;
                    self.layer = Layer::Inbox(0);
                }
                KeyCode::Enter => {
                    let Layer::Inbox(index) = self.layer else {
                        return false;
                    };
                    if let Some(row) = self.inbox_rows().get(index).cloned() {
                        self.inbox_selection = Some(row.key());
                        let request = match row {
                            InboxRow::Approval {
                                task_id,
                                session_id,
                                approval_id,
                            } => Request::OpenTask {
                                session: session_id,
                                task: task_id,
                                approval: Some(approval_id),
                            },
                            InboxRow::Task(task) => Request::OpenTask {
                                session: task.session_id,
                                task: task.id,
                                approval: None,
                            },
                        };
                        self.send(worker, request);
                    }
                }
                KeyCode::Up | KeyCode::Down | KeyCode::PageUp | KeyCode::PageDown => {
                    let rows = self.inbox_rows();
                    let Layer::Inbox(mut index) = self.layer else {
                        return false;
                    };
                    match key.code {
                        KeyCode::Up => index = index.saturating_sub(1),
                        KeyCode::Down => index = (index + 1).min(rows.len().saturating_sub(1)),
                        KeyCode::PageUp => index = index.saturating_sub(10),
                        KeyCode::PageDown => index = (index + 10).min(rows.len().saturating_sub(1)),
                        _ => {}
                    }
                    self.inbox_selection = rows.get(index).map(InboxRow::key);
                    self.layer = Layer::Inbox(index);
                    if self.inbox_filter == InboxFilter::Attention
                        && self.inbox_approvals_more
                        && index + 1 == rows.len()
                        && matches!(key.code, KeyCode::Down | KeyCode::PageDown)
                    {
                        self.send(worker, Request::MoreApprovals);
                    }
                }
                _ => {}
            }
        }
        false
    }

    fn history_key(&mut self, key: KeyEvent, worker: &Worker) -> bool {
        let pressed = key.kind == KeyEventKind::Press;
        if !pressed {
            return false;
        }
        if matches!(key.code, KeyCode::F(1) | KeyCode::Char('/')) {
            self.layer = Layer::Commands(0, String::new());
            return false;
        }
        if key.code == KeyCode::Char('q') {
            return true;
        }
        if !self.busy {
            let role = match key.code {
                KeyCode::Char('u') | KeyCode::Char('U') => Some(Some(SessionHistoryRole::User)),
                KeyCode::Char('a') | KeyCode::Char('A') => {
                    Some(Some(SessionHistoryRole::Assistant))
                }
                KeyCode::Char('b') | KeyCode::Char('B') => Some(None),
                _ => None,
            };
            if let Some(role) = role {
                self.search_history(role, worker);
                return false;
            }
            match key.code {
                KeyCode::Enter => {
                    let Layer::History(index) = self.layer else {
                        return false;
                    };
                    if let Some(item) = self
                        .history_page
                        .as_ref()
                        .and_then(|page| page.items.get(index))
                    {
                        let task = item.task.clone();
                        self.history_selection = Some(task.id.clone());
                        if self.send(
                            worker,
                            Request::OpenTask {
                                session: task.session_id,
                                task: task.id,
                                approval: None,
                            },
                        ) {
                            self.layer = Layer::None;
                        }
                    }
                }
                KeyCode::PageDown => {
                    if let Some(page) = &self.history_page
                        && let Some(before) = page.next_before_sequence
                    {
                        let request = Request::SearchHistory {
                            query: page.query.clone(),
                            role: page.role,
                            through_sequence: Some(page.through_sequence),
                            before_sequence: Some(before),
                            append: true,
                        };
                        self.send(worker, request);
                    }
                }
                KeyCode::Char('r') | KeyCode::Char('R') => {
                    self.search_history(self.history_role, worker)
                }
                KeyCode::Up | KeyCode::Down | KeyCode::PageUp => {
                    let len = self
                        .history_page
                        .as_ref()
                        .map_or(0, |page| page.items.len());
                    let Layer::History(mut index) = self.layer else {
                        return false;
                    };
                    match key.code {
                        KeyCode::Up => index = index.saturating_sub(1),
                        KeyCode::Down => index = (index + 1).min(len.saturating_sub(1)),
                        KeyCode::PageUp => index = index.saturating_sub(10),
                        _ => {}
                    }
                    self.history_selection = self
                        .history_page
                        .as_ref()
                        .and_then(|page| page.items.get(index))
                        .map(|item| item.task.id.clone());
                    self.layer = Layer::History(index);
                }
                _ => {}
            }
        }
        false
    }

    fn search_history(&mut self, role: Option<SessionHistoryRole>, worker: &Worker) {
        let query = self.history_query.clone();
        if self.send(
            worker,
            Request::SearchHistory {
                query,
                role,
                through_sequence: None,
                before_sequence: None,
                append: false,
            },
        ) {
            self.history_role = role;
            self.history_page = None;
            self.history_selection = None;
            self.layer = Layer::History(0);
        }
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
                    && self.send(
                        worker,
                        Request::Sessions(
                            session_catalog(
                                root.id.clone(),
                                0,
                                self.session_archived,
                                self.session_query.clone(),
                            ),
                            true,
                        ),
                    )
                {
                    self.layer = Layer::Sessions(None, 0);
                }
            }
            KeyCode::Char('I') if active => {
                self.inbox_filter = InboxFilter::Attention;
                self.inbox_selection = None;
                if self.send(worker, Request::WorkspaceInbox(true)) {
                    self.layer = Layer::Inbox(0);
                }
            }
            KeyCode::Char('b') if active && self.session.is_some() => self.draft_fork(None),
            KeyCode::Char('L') if active => {
                if let Some(session) = &self.session {
                    let browser = TaskBrowser::new(session.id.clone());
                    if let Some(params) = browser.query(PageAction::First)
                        && self.send(worker, Request::TaskBrowser(params, PageAction::First))
                    {
                        self.layer = Layer::TaskBrowser(browser);
                    }
                }
            }
            KeyCode::Char('Q') if active => {
                if let Some(session) = &self.session
                    && self.send(worker, Request::Queue(session.id.clone()))
                {
                    self.layer = Layer::Queue(None, 0);
                }
            }
            KeyCode::F(2) if active && self.session.is_some() => self.toggle_plan_mode(),
            KeyCode::F(3) if active => self.open_draft_context(worker),
            KeyCode::Char('H') if active => {
                self.input.clear();
                self.layer = Layer::Edit(Edit::HistorySearch);
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
            if self.session.is_some() {
                commands.push(("Browse session tasks", KeyCode::Char('L')));
                commands.push(("Queued prompts", KeyCode::Char('Q')));
                commands.push(("Branch current session", KeyCode::Char('b')));
                let label = if self.plan_only {
                    "Switch to execution mode"
                } else {
                    "Switch to plan mode"
                };
                commands.push((label, KeyCode::F(2)));
                commands.push(("Review draft context", KeyCode::F(3)));
            }
            commands.push(("Workspace inbox", KeyCode::Char('I')));
            commands.push(("Search workspace history", KeyCode::Char('H')));
            if let Some(detail) = &self.detail {
                let task = &detail.task;
                if pending_approval_id(task).is_some() {
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
                commands.push(("Task timeline", KeyCode::Char('t')));
                commands.push(("Context and memory usage", KeyCode::Char('c')));
                if task.status == TaskStatus::Completed {
                    commands.push(("Branch from completed task", KeyCode::Char('B')));
                }
                if Self::can_implement(detail) {
                    commands.push(("Draft implementation", KeyCode::Char('P')));
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
                if let Some(approval_id) = pending_approval_id(task) {
                    self.send(
                        worker,
                        Request::Approval(task.id.clone(), approval_id.to_owned()),
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
            KeyCode::Char('t') => {
                if self.send(
                    worker,
                    Request::Timeline(TaskTimelineParams {
                        task_id: task.id.clone(),
                        after: 0,
                        through_sequence: None,
                    }),
                ) {
                    self.layer = Layer::Timeline(None, 0);
                }
            }
            KeyCode::Char('c') => {
                if self.send(worker, Request::ContextAttribution(task.id.clone())) {
                    self.layer = Layer::Context(String::new(), 0);
                }
            }
            KeyCode::Char('B') if task.status == TaskStatus::Completed => {
                self.draft_fork(Some(task.id.clone()));
            }
            KeyCode::Char('P') if Self::can_implement(detail) => {
                if !self.draft.text().is_empty() || self.input_artifact.is_some() {
                    self.message = "Submit or clear the current draft first".into();
                    return;
                }
                let plan = detail.plan.as_ref().expect("checked for a published plan");
                let steps = plan
                    .steps
                    .iter()
                    .enumerate()
                    .map(|(i, step)| format!("{}. {}", i + 1, clean(&step.text)))
                    .collect::<Vec<_>>()
                    .join("\n");
                let text = format!(
                    "Implement the following plan from task {}.\n\n{steps}",
                    task.id
                );
                if self.draft.insert(&text, DRAFT_LIMIT) {
                    self.key = uuid::Uuid::new_v4().to_string();
                    self.plan_only = false;
                    self.layer = Layer::Edit(Edit::Task);
                    self.message =
                        "Review the implementation draft and press Ctrl+S to submit".into();
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

    fn draft_fork(&mut self, through_task_id: Option<String>) {
        let Some(session) = &self.session else { return };
        self.input.clear();
        self.layer = Layer::Edit(Edit::Fork(SessionForkParams {
            session_id: session.id.clone(),
            client_request_id: uuid::Uuid::new_v4().to_string(),
            title: None,
            through_task_id,
            through_sequence: None,
        }));
    }

    fn can_implement(detail: &Detail) -> bool {
        detail.task.status == TaskStatus::Completed
            && detail.plan.is_some()
            && detail
                .task
                .context
                .as_ref()
                .is_some_and(|c| c.options.plan_only)
    }

    fn submit(&mut self, edit: Edit, worker: &Worker, enqueue: bool) {
        let value = if matches!(edit, Edit::Task) {
            self.draft.text().trim()
        } else {
            self.input.text().trim()
        };
        if value.is_empty() && !matches!(edit, Edit::Fork(_) | Edit::SessionSearch) {
            return;
        }
        if self.busy {
            self.message = "Submit again when the current request finishes".into();
            return;
        }
        let value = value.to_owned();
        let starts = matches!(edit, Edit::Task);
        let history_search = matches!(edit, Edit::HistorySearch);
        let session_browse = matches!(edit, Edit::SessionSearch | Edit::RenameSession(_));
        let fork = matches!(edit, Edit::Fork(_));
        let history_query = history_search.then(|| value.clone());
        let request = match edit {
            Edit::Task => {
                let Some(session) = &self.session else { return };
                Request::Start {
                    session: session.id.clone(),
                    text: value,
                    key: self.key.clone(),
                    input: self.input_artifact.clone(),
                    plan_only: self.plan_only,
                    context: Box::new(self.draft_context.clone()),
                    enqueue,
                }
            }
            Edit::Workspace => Request::OpenWorkspace(value),
            Edit::Session => {
                let Some(workspace) = &self.workspace else {
                    return;
                };
                Request::NewSession(workspace.id.clone(), value)
            }
            Edit::SessionSearch => {
                let Some(workspace) = &self.workspace else {
                    return;
                };
                self.session_query = (!value.is_empty()).then_some(value);
                Request::Sessions(
                    session_catalog(
                        workspace.id.clone(),
                        0,
                        self.session_archived,
                        self.session_query.clone(),
                    ),
                    true,
                )
            }
            Edit::RenameSession(session) => Request::UpdateSession {
                catalog: session_catalog(
                    session.workspace_id.clone(),
                    0,
                    self.session_archived,
                    self.session_query.clone(),
                ),
                session,
                title: Some(value),
            },
            Edit::Fork(mut request) => {
                request.title = (!value.is_empty()).then_some(value);
                Request::ForkSession(request)
            }
            Edit::Answer(task, call, _) => Request::Answer(task, call, value),
            Edit::Steer(task) => Request::Steer(task, value),
            Edit::HistorySearch => Request::SearchHistory {
                query: value.clone(),
                role: None,
                through_sequence: None,
                before_sequence: None,
                append: false,
            },
            Edit::Download(id) => Request::Download(id, value.into()),
        };
        if self.send(worker, request) {
            if starts {
                self.start_pending = true;
            } else if history_search {
                self.history_query = history_query.unwrap_or_default();
                self.history_role = None;
                self.history_page = None;
                self.history_selection = None;
                self.layer = Layer::History(0);
            } else if session_browse {
                self.layer = Layer::Sessions(None, 0);
            } else if !fork {
                self.layer = Layer::None;
            }
        }
    }

    pub fn draw(&self, frame: &mut Frame<'_>) {
        let area = frame.area();
        frame.render_widget(Block::default().style(self.palette.base), area);
        let expanded = matches!(self.layer, Layer::Edit(Edit::Task));
        let full_view = matches!(
            self.layer,
            Layer::Inbox(_) | Layer::History(_) | Layer::TaskBrowser(_)
        );
        let editor_height = if full_view {
            0
        } else if expanded {
            (area.height / 4).clamp(2, 7)
        } else {
            2
        };
        let show_status = self.busy || !self.message.is_empty();
        let [top, middle, composer, bottom] = Layout::vertical([
            Constraint::Length(if area.height >= 10 { 2 } else { 1 }),
            Constraint::Min(0),
            Constraint::Length(editor_height),
            Constraint::Length(1 + u16::from(show_status && area.height >= 12)),
        ])
        .areas(area);
        let root = self
            .workspace
            .as_ref()
            .map_or_else(String::new, |root| display_path(&root.root));
        let session = if matches!(self.layer, Layer::Inbox(_)) {
            "Workspace inbox".into()
        } else if matches!(self.layer, Layer::History(_)) {
            "Workspace history".into()
        } else {
            self.session
                .as_ref()
                .map(session_name)
                .unwrap_or_else(|| "No session".into())
        };
        let state = if !self.online {
            "Offline"
        } else if self.demo {
            "Offline demo"
        } else {
            "Local"
        };
        let title_width = usize::from(area.width).saturating_sub(state.width() + 15);
        let breadcrumb = self
            .session
            .as_ref()
            .and_then(|session| session.fork_origin.as_ref())
            .map_or(root.clone(), |origin| {
                format!(
                    "Branch of {} · {} inherited turns · {root}",
                    clean(&origin.session_id)
                        .chars()
                        .take(12)
                        .collect::<String>(),
                    origin.inherited_turns
                )
            });
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(vec![
                    Span::styled(" FLIUNO ", self.palette.label),
                    Span::raw(format!("  {}", ellipsis(&session, title_width))),
                    Span::styled(format!("  ·  {state}"), self.palette.quiet),
                ]),
                Line::styled(
                    format!(
                        " {}",
                        path_tail(&breadcrumb, usize::from(area.width).saturating_sub(2))
                    ),
                    self.palette.quiet,
                ),
            ])
            .style(self.palette.rail),
            top,
        );
        if matches!(self.layer, Layer::Inbox(_)) {
            self.draw_inbox(frame, middle);
        } else if matches!(self.layer, Layer::History(_)) {
            self.draw_history(frame, middle);
        } else if let Layer::TaskBrowser(browser) = &self.layer {
            self.draw_task_browser(frame, middle, browser);
        } else if area.width >= 90 {
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
        if !full_view {
            self.draw_composer(frame, composer, expanded);
        }
        let message = if self.busy { "Working" } else { &self.message };
        let help = if expanded {
            if area.width < 65 {
                " Ctrl+S Send  Ctrl+E Queue  F2/F3 Mode/Context "
            } else {
                " Ctrl+S Submit  Ctrl+E Queue  F2 Mode  F3 Context  Esc Back "
            }
        } else if full_view {
            " Enter Open  Esc Back "
        } else if matches!(self.layer, Layer::None) {
            if area.width < 70 {
                " F1 Commands  Tab View "
            } else {
                " F1 Commands  n New  Tab View  Shift+L All tasks "
            }
        } else {
            " Esc Back "
        };
        let (help, notice) = if bottom.height >= 2 {
            (help, Some(message))
        } else if show_status {
            (message, None)
        } else {
            (help, None)
        };
        let max_width = usize::from(area.width);
        let mut lines = vec![Line::styled(ellipsis(help, max_width), self.palette.label)];
        if let Some(notice) = notice {
            let text = format!(" {}", clean(notice).replace('\n', " "));
            lines.push(Line::styled(ellipsis(&text, max_width), self.palette.quiet));
        }
        frame.render_widget(Paragraph::new(lines).style(self.palette.rail), bottom);
        self.draw_layer(frame, area);
    }

    fn draw_task_browser(&self, frame: &mut Frame<'_>, area: Rect, browser: &TaskBrowser) {
        let [title, list] =
            Layout::vertical([Constraint::Length(3), Constraint::Min(0)]).areas(area);
        let filter = browser.status().map_or("All states", |s| status(s).0);
        frame.render_widget(
            Paragraph::new(vec![
                Line::styled(
                    format!(
                        " Session tasks  ·  {filter}  ·  Page {}",
                        browser.cursors.len()
                    ),
                    self.palette.label,
                ),
                Line::styled(
                    " f Filter  PgUp Prev  PgDn Next  r Refresh",
                    self.palette.quiet,
                ),
            ])
            .style(self.palette.rail),
            title,
        );
        let Some(page) = &browser.page else {
            render_text(frame, list, "Loading tasks…", 0);
            return;
        };
        if page.items.is_empty() {
            render_text(frame, list, "No tasks match this filter.", 0);
            return;
        }
        let rows = page
            .items
            .iter()
            .map(|task| {
                let (label, color) = task_status(task);
                let prompt = task_preview(task);
                if area.width < 68 {
                    ListItem::new(vec![
                        Line::styled(label, self.palette.status(color)),
                        Line::raw(prompt),
                    ])
                } else {
                    ListItem::new(Line::from(vec![
                        Span::styled(format!("{label:<19}"), self.palette.status(color)),
                        Span::raw(prompt),
                    ]))
                }
            })
            .collect::<Vec<_>>();
        let mut state = ListState::default().with_selected(Some(browser.index));
        frame.render_stateful_widget(
            List::new(rows)
                .highlight_symbol("› ")
                .highlight_style(self.palette.choice),
            list,
            &mut state,
        );
    }

    fn draw_inbox(&self, frame: &mut Frame<'_>, area: Rect) {
        let rows = self.inbox_rows();
        let heading = match self.inbox_filter {
            InboxFilter::Attention => "Needs attention [1]",
            InboxFilter::Active => "Running [2]",
            InboxFilter::Recent => "Recent [3]",
        };
        let [header, list_area] =
            Layout::vertical([Constraint::Length(3), Constraint::Min(0)]).areas(area);
        let counts = self.inbox.as_ref().map(|inbox| &inbox.snapshot.task_counts);
        let mut summary = counts.map_or_else(
            || "Loading workspace…".to_owned(),
            |counts| {
                format!(
                    "{} approvals · {} running · {} waiting input · {} interrupted · {} ended",
                    self.inbox_approvals.len(),
                    counts.running,
                    counts.waiting_input,
                    counts.interrupted,
                    counts
                        .completed
                        .saturating_add(counts.failed)
                        .saturating_add(counts.cancelled),
                )
            },
        );
        if self.inbox_approvals_more {
            summary.push_str(" · more approvals: PgDn at list end");
        }
        frame.render_widget(
            Paragraph::new(vec![
                Line::styled(
                    format!("{heading}    1 Attention / 2 Running / 3 Recent"),
                    self.palette.label,
                ),
                Line::styled(summary, self.palette.quiet),
            ])
            .wrap(Wrap { trim: false }),
            header,
        );
        let items = rows
            .iter()
            .map(|row| {
                let (task, label, session_id) = match row {
                    InboxRow::Approval {
                        task_id,
                        session_id,
                        ..
                    } => {
                        let task = self
                            .inbox
                            .as_ref()
                            .and_then(|inbox| inbox.snapshot.recent_tasks.as_ref())
                            .and_then(|tasks| tasks.iter().find(|task| task.id == *task_id));
                        (task, "Approval", session_id.as_str())
                    }
                    InboxRow::Task(task) => {
                        let label = match task.status {
                            _ if task.model_outcome_unknown || task.agent_outcome_unknown => {
                                "Verify result"
                            }
                            TaskStatus::WaitingInput => "Clarification",
                            TaskStatus::Interrupted => "Resume check",
                            TaskStatus::Failed => "Failed",
                            _ => status(task.status).0,
                        };
                        (Some(task), label, task.session_id.as_str())
                    }
                };
                let title = task
                    .map(|task| clean(&task.input_preview))
                    .unwrap_or_else(|| "Task needs review".to_owned());
                ListItem::new(vec![
                    Line::raw(format!("{label}  ·  {title}")),
                    Line::styled(format!("Session {session_id}"), self.palette.quiet),
                ])
            })
            .collect::<Vec<_>>();
        if items.is_empty() {
            frame.render_widget(
                Paragraph::new(if self.busy {
                    "Refreshing…"
                } else {
                    "Nothing to do in this view."
                })
                .style(self.palette.quiet),
                list_area,
            );
        } else {
            let selected = match self.layer {
                Layer::Inbox(index) => index,
                _ => 0,
            };
            let mut state = ListState::default().with_selected(Some(selected.min(items.len() - 1)));
            frame.render_stateful_widget(
                List::new(items)
                    .highlight_symbol("› ")
                    .highlight_style(self.palette.choice),
                list_area,
                &mut state,
            );
        }
    }

    fn draw_history(&self, frame: &mut Frame<'_>, area: Rect) {
        let [header, list_area] =
            Layout::vertical([Constraint::Length(3), Constraint::Min(0)]).areas(area);
        let Some(page) = &self.history_page else {
            frame.render_widget(
                Paragraph::new(if self.busy {
                    "Searching workspace history…"
                } else {
                    "No results loaded"
                }),
                area,
            );
            return;
        };
        let role = match page.role {
            Some(SessionHistoryRole::User) => "user",
            Some(SessionHistoryRole::Assistant) => "assistant",
            None => "all",
        };
        frame.render_widget(
            Paragraph::new(vec![
                Line::styled(format!("Results for {:?} · {role} · seq {}", page.query, page.through_sequence), self.palette.label),
                Line::styled(format!("{} scanned · {} oversized skipped · U user / A assistant / B both / PgDn more", page.scanned_turns, page.skipped_oversized_turns), self.palette.quiet),
            ]).wrap(Wrap { trim: false }),
            header,
        );
        let items = page
            .items
            .iter()
            .map(|item| {
                ListItem::new(vec![
                    Line::raw(format!(
                        "{}  ·  {}",
                        status(item.task.status).0,
                        clean(&item.task.input_preview)
                    )),
                    Line::styled(
                        format!(
                            "Session {} · completion {}",
                            item.task.session_id, item.completion_sequence
                        ),
                        self.palette.quiet,
                    ),
                ])
            })
            .collect::<Vec<_>>();
        if items.is_empty() {
            frame.render_widget(
                Paragraph::new("No matching history.").style(self.palette.quiet),
                list_area,
            );
        } else {
            let selected = match self.layer {
                Layer::History(index) => index,
                _ => 0,
            };
            let mut state = ListState::default().with_selected(Some(selected.min(items.len() - 1)));
            frame.render_stateful_widget(
                List::new(items)
                    .highlight_symbol("› ")
                    .highlight_style(self.palette.choice),
                list_area,
                &mut state,
            );
        }
    }

    fn draw_draft_context(
        &self,
        frame: &mut Frame<'_>,
        area: Rect,
        preview: Option<&TaskContext>,
        scroll: u16,
    ) {
        let options = &self.draft_context;
        let isolated = options.mode == ContextMode::Isolated;
        let scope = if isolated {
            "Isolated prompt"
        } else {
            "Session history"
        };
        let mut settings = vec![Line::styled(
            format!("s  Scope    {scope}"),
            self.palette.label,
        )];
        if !isolated {
            let memory = if options.disable_memory {
                "Off"
            } else {
                "Automatic"
            };
            let history = match options.detail {
                ContextDetail::Full => "Full records",
                ContextDetail::Conversation => "Conversation only",
            };
            let pin = options.through_sequence.map_or_else(
                || "Live".into(),
                |sequence| format!("Pinned at #{sequence}"),
            );
            settings.extend([
                Line::raw(format!("m  Memory   {memory}")),
                Line::raw(format!("h  History  {history}")),
                Line::raw(format!("p  Preview  {pin}")),
            ]);
        }
        settings.push(Line::styled("r  Refresh", self.palette.quiet));
        let [controls, result] = Layout::vertical([
            Constraint::Length(settings.len() as u16 + 1),
            Constraint::Min(0),
        ])
        .areas(area);
        frame.render_widget(Paragraph::new(settings), controls);
        let text =
            format_draft_context(preview, self.busy, &self.message, usize::from(result.width));
        render_text(frame, result, &text, scroll);
    }

    fn draw_composer(&self, frame: &mut Frame<'_>, area: Rect, focused: bool) {
        if area.width < 4 || area.height == 0 {
            return;
        }
        let mode = if self.plan_only {
            "Plan only"
        } else if focused {
            "New task · Execution"
        } else {
            "Draft · Execution"
        };
        let title = if self.session.is_none() {
            " No session ".to_owned()
        } else {
            let mut title = format!(" {mode} ");
            if self.draft_context.mode == ContextMode::Isolated {
                title.push_str("· Isolated ");
            } else if self.draft_context.through_sequence.is_some() {
                title.push_str("· History pinned ");
            } else if self.draft_context.disable_memory {
                title.push_str("· Memory off ");
            }
            if self.input_artifact.is_some() {
                title.push_str(" · Attachment ");
            }
            title
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
        if self.draft.text().is_empty() && !focused {
            if self.session.is_some() {
                frame.render_widget(
                    Paragraph::new("Press n to write a task").style(self.palette.quiet),
                    body,
                );
            }
        } else {
            frame.render_widget(
                Paragraph::new(lines.into_iter().map(Line::raw).collect::<Vec<_>>()),
                body,
            );
        }
        if focused && !self.start_pending {
            frame.set_cursor_position(Position::new(body.x + x, body.y + y));
        }
    }

    fn draw_tasks(&self, frame: &mut Frame<'_>, area: Rect, compact: bool) {
        let tasks = self.tasks();
        let title = if compact { "Session tasks" } else { "Tasks" };
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
                let (label, color) = task_status(task);
                let content = ellipsis(
                    &task_preview(task),
                    usize::from(area.width).saturating_sub(3),
                );
                ListItem::new(vec![
                    Line::raw(content),
                    Line::styled(label, self.palette.status(color)),
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
            if task
                .context
                .as_ref()
                .is_some_and(|context| context.options.plan_only)
            {
                lines.push(Line::styled("Plan only", self.palette.quiet));
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
            lines.push(Line::raw(if self.busy && self.selected.is_some() {
                "Loading task…"
            } else if self.tasks().is_empty() {
                "Press n to start work in this session"
            } else {
                "Select a task from the list"
            }));
        }
        frame.render_widget(
            Paragraph::new(lines)
                .wrap(Wrap { trim: false })
                .scroll((self.scroll, 0)),
            area,
        );
    }

    fn draw_layer(&self, frame: &mut Frame<'_>, area: Rect) {
        if matches!(
            self.layer,
            Layer::None
                | Layer::Inbox(_)
                | Layer::History(_)
                | Layer::TaskBrowser(_)
                | Layer::Edit(Edit::Task)
        ) {
            return;
        }
        let max_width = match self.layer {
            Layer::Commands(..) => 66,
            Layer::Edit(_) | Layer::ConfirmCancel(_) => 76,
            Layer::Approval { .. } => 88,
            _ => 112,
        };
        let modal = area.inner(Margin::new(
            (area.width.saturating_sub(max_width) / 2).max(u16::from(area.width >= 24)),
            if area.height < 12 { 0 } else { area.height / 9 },
        ));
        frame.render_widget(Clear, modal);
        let (title, actions) = match &self.layer {
            Layer::None | Layer::TaskBrowser(_) => return,
            Layer::Commands(_, query) => (
                format!("Commands  /  {query}"),
                "Type to filter  ·  Enter run  ·  Esc back",
            ),
            Layer::Inbox(_) => (
                "Workspace inbox".into(),
                "1 attention / 2 running / 3 recent / Enter open / h history",
            ),
            Layer::History(_) => (
                "Workspace history".into(),
                "Enter open / PgDn more / U user / A assistant / B both",
            ),
            Layer::Workspaces(..) => ("Workspaces".into(), "Open folder"),
            Layer::Sessions(..) => (
                format!(
                    "{}{}",
                    if self.session_archived {
                        "Archived sessions"
                    } else {
                        "Sessions"
                    },
                    self.session_query
                        .as_deref()
                        .map_or_else(String::new, |q| format!("  /  {}", clean(q)))
                ),
                "Enter open / f find / A archive view / r rename / x archive or restore / n new",
            ),
            Layer::Artifacts(_) => ("Artifacts".into(), ""),
            Layer::Timeline(page, _) => (
                page.as_ref().map_or_else(
                    || "Task timeline".into(),
                    |p| format!("Task timeline · through #{}", p.through_sequence),
                ),
                "Enter details / a approval / PgDn more / Home first / r refresh",
            ),
            Layer::TimelineEvent { sequence, .. } => (format!("Event #{sequence}"), "Esc timeline"),
            Layer::Context(..) => ("Context and memory usage".into(), "Esc back"),
            Layer::DraftContext { .. } => ("Draft context".into(), "Esc Return to draft"),
            Layer::Queue(queue, _) => (
                queue.as_ref().map_or_else(
                    || "Queued prompts".into(),
                    |q| {
                        let state = if q.archived {
                            " · Archived"
                        } else if q.blocking_task_id.is_some() {
                            " · Task in progress"
                        } else {
                            ""
                        };
                        format!("Queued prompts ({}){state}", q.pending.len())
                    },
                ),
                "Enter start first / Del remove / g blocking task / r refresh",
            ),
            Layer::Approval { allow_once, .. } => (
                "Review operation".into(),
                if *allow_once {
                    "y Allow once  ·  n Deny  ·  Esc Back"
                } else {
                    "n Deny  ·  Esc Back"
                },
            ),
            Layer::ConfirmCancel(_) => {
                ("Cancel task?".into(), "y Cancel task  ·  Esc Keep running")
            }
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
        let inset = u16::from(area.width >= 55 || !matches!(self.layer, Layer::Approval { .. }));
        let body = block.inner(modal).inner(Margin::new(inset, 0));
        frame.render_widget(block, modal);
        if body.is_empty() {
            return;
        }
        let footer_height = if actions.is_empty() || body.height < 2 {
            0
        } else if body.height >= 4 && actions.width() > usize::from(body.width) {
            2
        } else {
            1
        };
        let [body, footer] =
            Layout::vertical([Constraint::Min(0), Constraint::Length(footer_height)]).areas(body);
        frame.render_widget(
            Paragraph::new(actions)
                .wrap(Wrap { trim: true })
                .style(self.palette.label),
            footer,
        );
        match &self.layer {
            Layer::None => {}
            Layer::Inbox(_) | Layer::History(_) | Layer::TaskBrowser(_) => {}
            Layer::DraftContext { preview, scroll } => {
                self.draw_draft_context(frame, body, preview.as_deref(), *scroll)
            }
            Layer::Commands(index, query) => render_list(
                frame,
                body,
                self.filtered_commands(query)
                    .into_iter()
                    .map(|(label, key)| format!("{:<10}{label}", key_label(key)))
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
            Layer::Timeline(page, index) => render_list(
                frame,
                body,
                page.as_ref().map_or_else(Vec::new, |page| {
                    page.entries
                        .iter()
                        .map(|entry| {
                            let label = clean(&entry.kind.replace('_', " "));
                            let tool = entry
                                .tool_name
                                .as_ref()
                                .map_or_else(String::new, |name| format!("  ·  {}", clean(name)));
                            let approval = entry
                                .approval_id
                                .as_ref()
                                .map_or_else(String::new, |id| format!("  ·  {}", clean(id)));
                            format!("#{}  {label}{tool}{approval}", entry.sequence)
                        })
                        .collect()
                }),
                *index,
                self.palette.choice,
            ),
            Layer::TimelineEvent { text, scroll, .. } | Layer::Context(text, scroll) => {
                render_text(frame, body, text, *scroll)
            }
            Layer::Queue(queue, index) => render_list(
                frame,
                body,
                queue.as_ref().map_or_else(Vec::new, |queue| {
                    queue
                        .pending
                        .iter()
                        .map(|item| {
                            let mode = if item.plan_only { "Plan" } else { "Execution" };
                            let text = clean(&item.input_preview).replace('\n', " ");
                            format!("{mode}  ·  {text}")
                        })
                        .collect()
                }),
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

fn format_draft_context(
    preview: Option<&TaskContext>,
    busy: bool,
    message: &str,
    width: usize,
) -> String {
    let Some(preview) = preview else {
        return if busy {
            "Reading session history…"
        } else {
            message
        }
        .to_owned();
    };
    if preview.options.mode == ContextMode::Isolated {
        return "This prompt starts without session history or workspace memory.".into();
    }
    let mut lines = vec![format!(
        "{} completed turns · {} omitted · through #{}",
        preview.turns.len(),
        preview.omitted_turns,
        preview.through_sequence,
    )];
    if !preview.options.disable_memory {
        lines.push("Automatic memory is selected when the task starts.".into());
    }
    lines.push(String::new());
    for turn in preview.turns.iter().rev() {
        let prompt = turn.messages.iter().find_map(|message| match message {
            ContextMessage::User { text } => Some(text),
            _ => None,
        });
        let title = prompt.map_or_else(
            || "Completed task".to_owned(),
            |text| clean(text).split_whitespace().collect::<Vec<_>>().join(" "),
        );
        lines.push(ellipsis(
            &format!("#{}  {title}", turn.completion_sequence),
            width,
        ));
    }
    lines.join(
        "
",
    )
}

fn format_context_attribution(report: &TaskContextAttribution) -> String {
    let mut lines = vec![
        format!("{} model attempts", report.model_steps),
        format!(
            "{} history turns · {} omitted · {} summarized",
            report.history.len(),
            report.omitted_history,
            report.summarized_history.len()
        ),
        format!(
            "{} memories · {} omitted for budget",
            report.memories.len(),
            report
                .memories
                .iter()
                .filter(|item| item.omitted_for_budget)
                .count()
        ),
        format!("{} B of workspace instructions", report.instructions_bytes),
    ];
    if let Some(bytes) = &report.normalized_input_bytes {
        lines.push(format!("\nNormalized fragments (bytes)\nCurrent text {} · media {}\nHistory {} · memory {}\nTool definitions {} · results {} · provider replay {}",
            bytes.current_text, bytes.current_media, bytes.history, bytes.memory,
            bytes.tool_definitions, bytes.tool_results, bytes.provider_replay));
    }
    if let Some(EventKind::ModelPreflightObserved {
        model,
        estimated_input_tokens,
        safety_margin_tokens,
        observed_input_tokens,
        ..
    }) = report.latest_preflight.as_ref().map(|event| &event.data)
    {
        let observed = observed_input_tokens
            .map_or_else(String::new, |tokens| format!(" · reported {tokens}"));
        lines.push(format!("\n{} · estimated {estimated_input_tokens} tokens · margin {safety_margin_tokens}{observed}", clean(model)));
    }
    if let Some(EventKind::ModelBudgetRejected { report, .. }) = report
        .latest_budget_rejection
        .as_ref()
        .map(|event| &event.data)
    {
        lines.push(format!(
            "\nBudget rejection · {:?} · observed {} · limit {}",
            report.kind, report.observed, report.limit
        ));
    }
    if !report.memories.is_empty() {
        lines.push("\nMemory sources".into());
        for source in &report.memories {
            lines.push(format!(
                "{} · revision {} · {} B{}",
                clean(&source.id),
                source.revision,
                source.normalized_bytes,
                if source.omitted_for_budget {
                    " · omitted"
                } else {
                    ""
                }
            ));
        }
    }
    lines.join("\n")
}

fn artifact_id(value: &ArtifactSummary) -> ArtifactGetParams {
    ArtifactGetParams {
        task_id: value.task_id.clone(),
        artifact_id: value.id.clone(),
    }
}

fn session_catalog(
    workspace_id: String,
    after: u64,
    archived: bool,
    query: Option<String>,
) -> SessionListParams {
    SessionListParams {
        workspace_id,
        after,
        archived: Some(archived),
        query,
        order: Some(SessionListOrder::NewestFirst),
        limit: 30,
    }
}

fn sessions(workspace: String, after: u64, browse: bool) -> Request {
    Request::Sessions(session_catalog(workspace, after, false, None), browse)
}

fn edit_limit(edit: &Edit) -> usize {
    match edit {
        Edit::Task => DRAFT_LIMIT,
        Edit::Session | Edit::SessionSearch | Edit::RenameSession(_) | Edit::Fork(_) => 480,
        Edit::HistorySearch => 256,
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

fn key_label(key: KeyCode) -> String {
    match key {
        KeyCode::Char(c) if c.is_ascii_uppercase() => format!("Shift+{c}"),
        KeyCode::Char(c) => c.to_string(),
        KeyCode::F(n) => format!("F{n}"),
        _ => String::new(),
    }
}

fn display_path(path: &str) -> String {
    if let Some(unc) = path.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{}", clean(unc))
    } else {
        clean(path.strip_prefix(r"\\?\").unwrap_or(path))
    }
}

fn ellipsis(text: &str, width: usize) -> String {
    if text.width() <= width {
        return text.to_owned();
    }
    if width == 0 {
        return String::new();
    }
    let mut out = String::new();
    let mut remaining = width - 1;
    for cluster in text.graphemes(true) {
        let cells = cluster.width();
        if cells > remaining {
            break;
        }
        out.push_str(cluster);
        remaining -= cells;
    }
    out.push('…');
    out
}

fn path_tail(path: &str, width: usize) -> String {
    if path.width() <= width {
        return path.to_owned();
    }
    if width == 0 {
        return String::new();
    }
    let mut start = path.len();
    let mut remaining = width - 1;
    for (index, cluster) in path.grapheme_indices(true).rev() {
        let cells = cluster.width();
        if cells > remaining {
            break;
        }
        start = index;
        remaining -= cells;
    }
    format!("…{}", &path[start..])
}

fn session_name(session: &Session) -> String {
    clean(session.title.as_deref().unwrap_or("Untitled session"))
}

fn task_status(task: &TaskSummary) -> (&'static str, Color) {
    if task.agent_outcome_unknown || task.model_outcome_unknown {
        ("Unknown", Color::Yellow)
    } else {
        status(task.status)
    }
}

fn task_preview(task: &TaskSummary) -> String {
    let preview = clean(&task.input_preview).replace('\n', " ");
    if preview.trim().is_empty() {
        task.id.clone()
    } else {
        preview
    }
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

fn pending_approval_id(task: &Task) -> Option<&str> {
    task.pending_approval
        .as_ref()
        .map(|approval| approval.id.as_str())
        .or_else(|| {
            task.agent_run
                .as_ref()?
                .pending_approvals
                .first()
                .map(|approval| approval.id.as_str())
        })
}

fn approval_review(
    task: &str,
    id: &str,
    root: &str,
    details: ApprovalDetails,
) -> Option<(String, bool)> {
    match details {
        ApprovalDetails::ToolCall { approval } => {
            if approval.task_id != task || approval.id != id {
                return None;
            }
            let arguments =
                serde_json::to_string_pretty(&approval.call.arguments).unwrap_or_default();
            let review = approval
                .execution_plan
                .as_ref()
                .and_then(|plan| serde_json::to_string_pretty(plan).ok())
                .unwrap_or_default();
            Some((
                format!(
                    "{}\n\nWorkspace\n{root}\n\nArguments\n{arguments}\n{review}",
                    approval.call.name
                ),
                true,
            ))
        }
        ApprovalDetails::AgentPermission { ref approval }
        | ApprovalDetails::HostTool { ref approval } => {
            if approval.id != id {
                return None;
            }
            let label = match details {
                ApprovalDetails::HostTool { .. } => "Agent host tool",
                _ => "External Agent permission",
            };
            let request = serde_json::to_string_pretty(&approval.request).unwrap_or_default();
            Some((
                format!(
                    "{label}\nWorkspace: {root}\nRun: {}\n\n{request}",
                    approval.run_id
                ),
                approval.allow_once_available,
            ))
        }
    }
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
            fork_origin: None,
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
                allow_once: true,
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
                            .contains("y Allow once")
                    })
                    .expect("approval actions stay visible");
                let label = footer.iter().find(|cell| cell.symbol() == "A").unwrap();
                assert_eq!(label.fg, app.palette.label.fg.unwrap_or(Color::Reset));
                assert_eq!(label.bg, app.palette.rail.bg.unwrap_or(Color::Reset));
            }
        }
    }

    #[test]
    fn approval_review_honors_agent_decision_options() {
        let mut approval = fliuno_protocol::AgentApproval {
            id: "approval".into(),
            run_id: "run".into(),
            permission_id: "permission".into(),
            source: fliuno_protocol::AgentApprovalSource::AgentPermission,
            request: serde_json::json!({"title": "Read report"}),
            allow_once_available: false,
            expires_at_unix_ms: u64::MAX,
        };
        let details = ApprovalDetails::AgentPermission {
            approval: approval.clone(),
        };
        assert!(approval_review("task", "different", "root", details.clone()).is_none());
        let (text, allow_once) = approval_review("task", "approval", "root", details).unwrap();
        assert!(text.contains("External Agent permission"));
        assert!(text.contains("Read report"));
        assert!(!allow_once);

        approval.allow_once_available = true;
        approval.source = fliuno_protocol::AgentApprovalSource::HostTool;
        let details = ApprovalDetails::HostTool { approval };
        let (text, allow_once) = approval_review("task", "approval", "root", details).unwrap();
        assert!(text.contains("Agent host tool"));
        assert!(allow_once);
    }

    #[test]
    fn workspace_inbox_filters_preserve_actionable_state_and_fit_supported_widths() {
        let task = |id: &str, status| TaskSummary {
            id: id.into(),
            session_id: format!("session-{id}"),
            status,
            backend: fliuno_protocol::TaskBackend::Model,
            input_preview: format!("request {id}"),
            pending_approval_count: u32::from(status == TaskStatus::WaitingApproval),
            model_outcome_unknown: false,
            agent_outcome_unknown: false,
        };
        let approval_task = task("approval", TaskStatus::WaitingApproval);
        let running = task("running", TaskStatus::Running);
        let input = task("input", TaskStatus::WaitingInput);
        let completed = task("done", TaskStatus::Completed);
        let approval = ApprovalSummary {
            id: "approval-id".into(),
            task_id: approval_task.id.clone(),
            session_id: approval_task.session_id.clone(),
            kind: fliuno_protocol::ApprovalKind::ToolCall,
            allow_once_available: true,
            expires_at_unix_ms: None,
        };
        let snapshot = WorkSnapshot {
            task_counts: fliuno_protocol::TaskStatusCounts {
                running: 1,
                waiting_approval: 1,
                waiting_input: 1,
                completed: 1,
                ..Default::default()
            },
            approvals: fliuno_protocol::ApprovalInboxPage {
                snapshot: fliuno_protocol::ApprovalInboxSnapshot {
                    through_sequence: 12,
                    captured_at_unix_ms: 100,
                },
                page: ObjectPage {
                    items: vec![approval.clone()],
                    next_after: 1,
                    has_more: true,
                },
            },
            recent_tasks: Some(vec![approval_task, running, input, completed]),
            next_actions: Some(vec![fliuno_protocol::WorkNextAction {
                task_id: "input".into(),
                kind: WorkActionKind::AnswerInput,
            }]),
        };
        let mut app = App::new(true, Theme::Mono);
        app.workspace = Some(Workspace {
            id: "workspace".into(),
            root: "project".into(),
        });
        app.update_inbox(WorkspaceInbox { snapshot });
        app.layer = Layer::Inbox(0);
        let rows = app.inbox_rows();
        assert_eq!(rows.len(), 2);
        assert!(matches!(&rows[0], InboxRow::Approval { task_id, .. } if task_id == "approval"));
        assert!(matches!(&rows[1], InboxRow::Task(task) if task.id == "input"));
        assert!(app.inbox_approvals_more);

        app.inbox_filter = InboxFilter::Active;
        assert!(
            matches!(app.inbox_rows().as_slice(), [InboxRow::Task(task)] if task.id == "running")
        );
        app.inbox_filter = InboxFilter::Recent;
        assert!(matches!(app.inbox_rows().as_slice(), [InboxRow::Task(task)] if task.id == "done"));
        app.inbox_filter = InboxFilter::Attention;
        app.layer = Layer::Inbox(0);
        for (width, height) in [(48, 18), (80, 24), (120, 32)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            let frame = terminal.draw(|frame| app.draw(frame)).unwrap();
            let text = frame
                .buffer
                .content()
                .iter()
                .map(|cell| cell.symbol())
                .collect::<String>();
            assert!(
                text.contains("Workspace inbox"),
                "missing inbox at {width}x{height}"
            );
            assert!(
                text.contains("Needs attention"),
                "missing filter at {width}x{height}"
            );
            assert!(
                text.contains("request input"),
                "missing actionable task at {width}x{height}"
            );
        }
    }

    #[test]
    fn workspace_history_view_shows_watermark_and_match_at_supported_widths() {
        let task = TaskSummary {
            id: "task".into(),
            session_id: "session".into(),
            status: TaskStatus::Completed,
            backend: fliuno_protocol::TaskBackend::Model,
            input_preview: "Rust workspace indexing".into(),
            pending_approval_count: 0,
            model_outcome_unknown: false,
            agent_outcome_unknown: false,
        };
        let mut app = App::new(true, Theme::Mono);
        app.layer = Layer::History(0);
        app.history_page = Some(HistoryPage {
            query: "workspace".into(),
            role: None,
            through_sequence: 88,
            next_before_sequence: Some(55),
            scanned_turns: 16,
            skipped_oversized_turns: 1,
            items: vec![crate::service::HistoryItem {
                task,
                completion_sequence: 63,
            }],
        });
        for (width, height) in [(48, 18), (80, 24), (120, 32)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            let frame = terminal.draw(|frame| app.draw(frame)).unwrap();
            let text = frame
                .buffer
                .content()
                .iter()
                .map(|cell| cell.symbol())
                .collect::<String>();
            assert!(
                text.contains("Workspace history"),
                "missing history at {width}x{height}"
            );
            assert!(
                text.contains("88"),
                "missing pinned sequence at {width}x{height}"
            );
            assert!(
                text.contains("Rust workspace indexing"),
                "missing result at {width}x{height}"
            );
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
            fork_origin: None,
        };
        app.select_session(session("one"));
        app.layer = Layer::Edit(Edit::Task);
        app.paste("draft");
        app.toggle_plan_mode();
        let key = app.key.clone();
        app.input_artifact = Some(ArtifactGetParams {
            task_id: "task".into(),
            artifact_id: "artifact".into(),
        });
        app.select_session(session("two"));
        assert_eq!(app.draft.text(), "");
        assert!(app.input_artifact.is_none());
        assert!(!app.plan_only);
        app.select_session(session("one"));
        assert!(app.plan_only);
        assert_eq!(app.draft.text(), "draft");
        assert_eq!(app.key, key);
        assert_eq!(app.input_artifact.unwrap().artifact_id, "artifact");
    }
}
