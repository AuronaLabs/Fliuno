mod answer;
mod commands;
mod deliverables;
mod memories;
mod model_picker;
mod palette;
mod panels;
pub(crate) mod setup;
mod transcript;
mod workbench;

use commands::{Command, CommandAction, CommandMenu, CommandOrigin};
use palette::Palette;
use transcript::Transcript;

use crate::{
    activity,
    backup::{BackupRequest, BackupResult},
    editor::Editor,
    options::Theme,
    recovery::{ReconcileForm, disposition_text, outcome_label},
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
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap},
};
use std::{cell::Cell, path::PathBuf};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

const ACCENT: Color = Color::Cyan;
use fliuno_protocol::{
    ContextDetail, ContextMessage, ContextMode, ContextOptions, EventKind, SessionContextParams,
    SessionForkParams, SessionListOrder, SessionListParams, TaskContext, TaskContextAttribution,
    TaskQueue, TaskQueueItemParams, TaskQueueState, TaskTimelinePage, TaskTimelineParams,
};

const DRAFT_LIMIT: usize = 48 * 1024;

enum Layer {
    Commands(CommandMenu),
    ModelPicker(model_picker::Picker),
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
    Recovery(String, u16),
    Reconcile(Box<ReconcileForm>),
    Backup(Option<Box<BackupResult>>),
    Diagnostics(String, u16),
    Memories(Box<memories::MemoryPanel>),
    Deliverables(Box<deliverables::Catalog>),
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
    BackupCreate,
    BackupVerify,
    BackupRestoreSource,
    BackupRestoreDestination(PathBuf),
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
            Self::BackupCreate => "Create backup in a new folder",
            Self::BackupVerify => "Verify backup folder",
            Self::BackupRestoreSource => "Restore from backup folder",
            Self::BackupRestoreDestination(_) => "Restore to a new database path",
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
    model: Option<String>,
    model_profiles: bool,
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
    activity: Vec<activity::Preview>,
    scroll: Cell<u16>,
    scroll_anchor: Cell<Option<transcript::Anchor>>,
    transcript: Transcript,
    rendered_scroll: Cell<u16>,
    follow: bool,
    show_details: bool,
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
            model: None,
            model_profiles: false,
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
            activity: Vec::new(),
            scroll: Cell::new(0),
            scroll_anchor: Cell::new(None),
            transcript: Transcript::default(),
            rendered_scroll: Cell::new(0),
            follow: false,
            show_details: false,
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
        self.transcript = Transcript::default();
        self.set_conversation_scroll(0);
        self.follow = !self.demo;
        self.show_details = false;
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

    fn set_conversation_scroll(&self, row: u16) {
        self.scroll.set(row);
        self.scroll_anchor.set(None);
    }

    fn select_task(&mut self, id: String) {
        self.next = Some(Request::Task(id.clone()));
        self.selected = Some(id);
        self.detail = None;
        self.set_conversation_scroll(0);
        self.follow = false;
        self.show_details = false;
    }

    fn update(&mut self, update: Update) {
        match update {
            Update::Online {
                workspace,
                model,
                model_profiles,
            } => {
                self.online = true;
                self.model = model.and_then(model_label);
                self.model_profiles = model_profiles;
                self.activity.clear();
                self.message.clear();
                let id = self.workspace.get_or_insert(workspace).id.clone();
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
                self.transcript = Transcript::default();
                self.input.clear();
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
                } else if self.demo {
                    self.layer = Layer::Edit(Edit::Session);
                } else {
                    self.next = Some(Request::NewSession(workspace, Some("Conversation".into())));
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
                    self.show_details = true;
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
                        self.follow = !self.demo;
                    } else if previous != Some(current)
                        && let Some(id) = &self.selected
                    {
                        self.next = Some(Request::Task(id.clone()));
                    } else if !self.demo && previous.is_none() && self.tasks().is_empty() {
                        self.layer = Layer::Edit(Edit::Task);
                    }
                }
            }
            Update::Transcript(session, page) => {
                let caught_up = matches!(page, crate::service::TranscriptPage::CaughtUp(_));
                if self
                    .session
                    .as_ref()
                    .is_some_and(|active| active.id == session)
                    && self.transcript.install(page)
                    && !self.show_details
                {
                    self.set_conversation_scroll(0);
                    if caught_up && !self.follow {
                        self.message = "Conversation updated. PageUp loads older turns.".into();
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
                        self.follow = true;
                    }
                    let restore_composer = !self.demo
                        && self.follow
                        && !self.show_details
                        && matches!(self.layer, Layer::None)
                        && detail.task.status == TaskStatus::Completed
                        && (self.start_acknowledged
                            || self.detail.as_ref().is_some_and(|previous| {
                                previous.task.id == detail.task.id
                                    && !previous.task.status.is_terminal()
                            }));
                    self.detail = Some(detail);
                    if restore_composer {
                        self.layer = Layer::Edit(Edit::Task);
                    }
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
                if self.pending_preview.take().is_some_and(|expected| {
                    expected.task_id == preview.id.task_id
                        && expected.artifact_id == preview.id.artifact_id
                }) {
                    if matches!(self.layer, Layer::Deliverables(_)) {
                        self.install_catalog_preview(preview);
                    } else if matches!(self.layer, Layer::Artifacts(_)) {
                        self.layer = Layer::Preview(preview, 0);
                    }
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
                self.set_conversation_scroll(0);
                self.message = if self.demo {
                    "Task submitted".into()
                } else {
                    String::new()
                };
            }
            Update::Notice(message) | Update::Error(message) => self.message = message,
            Update::Offline(message) => {
                self.online = false;
                self.model = None;
                self.model_profiles = false;
                self.activity.clear();
                self.layer = Layer::None;
                self.next = None;
                self.message = message;
            }
            Update::Activity(previews) => self.activity = previews,
            Update::ModelProfiles(profiles) => {
                if let Layer::ModelPicker(picker) = &mut self.layer {
                    picker.install(profiles);
                }
            }
            Update::ModelSelected(model) => {
                self.model = model_label(model.model);
                if let Layer::ModelPicker(picker) = &self.layer {
                    self.layer = picker.return_layer();
                }
                self.message = self
                    .model
                    .as_ref()
                    .map_or_else(String::new, |name| format!("Using {name}"));
            }
            Update::ModelProfileError(error) => {
                if let Layer::ModelPicker(picker) = &mut self.layer {
                    picker.error = Some(clean(&error));
                }
            }
            Update::Memories {
                workspace,
                query,
                page,
            } => self.install_memories(&workspace, &query, page),
            Update::MemorySaved(workspace) => self.memory_saved(&workspace),
            Update::Deliverables { workspace, page } => self.install_deliverables(&workspace, page),
            Update::Saved { path, sha256 } => {
                self.message = if matches!(self.layer, Layer::Deliverables(_)) {
                    format!("Saved {}\nVerified SHA-256: {sha256}", path.display())
                } else {
                    format!("Saved {}", path.display())
                };
            }
            Update::Diagnostics(text) => {
                if matches!(self.layer, Layer::Diagnostics(..)) {
                    self.layer = Layer::Diagnostics(text, 0);
                }
            }
            Update::BackupDone(result) => {
                self.message = result.request.label().into();
                self.layer = Layer::Backup(Some(result));
            }
            Update::Reconciled(task) => {
                if matches!(&self.layer, Layer::Reconcile(form) if form.task_id == task) {
                    self.layer = Layer::Recovery(task, 0);
                }
                self.message = "External outcome recorded".into();
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
        } else if let Layer::Deliverables(catalog) = &self.layer {
            if !self.busy {
                catalog.paste(&mut self.input, value);
            }
        } else if let Layer::Memories(panel) = &mut self.layer {
            if !self.busy {
                panel.paste(value);
            }
        } else if let Layer::Reconcile(form) = &mut self.layer {
            if !self.busy {
                form.insert(&clean(value));
            }
        } else if let Layer::Commands(menu) = &mut self.layer {
            if menu.origin == CommandOrigin::Slash {
                let text = format!("/{}{value}", menu.query);
                self.layer = Layer::Edit(Edit::Task);
                self.paste(&text);
            } else {
                push_bounded(&mut menu.query, &clean(value).replace('\n', " "), 128);
                menu.index = 0;
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
        if matches!(self.layer, Layer::Deliverables(_)) {
            self.catalog_key(key, worker);
            return false;
        }
        if matches!(self.layer, Layer::Memories(_)) {
            self.memories_key(key, worker);
            return false;
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
            if pressed && !self.busy && matches!(key.code, KeyCode::Char('v') | KeyCode::F(6)) {
                self.open_memories(worker, true);
                return false;
            }
            if key.code == KeyCode::Esc || key.code == KeyCode::F(3) {
                self.layer = Layer::Edit(Edit::Task);
            } else {
                self.draft_context_key(key, worker);
            }
            return false;
        }
        if key.code == KeyCode::Esc
            && let Layer::Reconcile(form) = &self.layer
        {
            self.layer = Layer::Recovery(form.task_id.clone(), 0);
            return false;
        }
        if key.code == KeyCode::Esc
            && matches!(
                self.layer,
                Layer::Edit(
                    Edit::BackupCreate
                        | Edit::BackupVerify
                        | Edit::BackupRestoreSource
                        | Edit::BackupRestoreDestination(_)
                )
            )
        {
            self.layer = Layer::Backup(None);
            return false;
        }
        if key.code == KeyCode::Esc
            && let Layer::Commands(menu) = &self.layer
            && menu.origin != CommandOrigin::Workbench
        {
            self.layer = Layer::Edit(Edit::Task);
            return false;
        }
        if matches!(self.layer, Layer::ModelPicker(_)) {
            self.model_picker_key(key, worker);
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
            let composing = matches!(edit, Edit::Task);
            if composing && pressed && key.code == KeyCode::F(1) {
                self.layer = Layer::Commands(CommandMenu::new(CommandOrigin::Composer));
                return false;
            }
            if composing
                && pressed
                && key.code == KeyCode::Char('/')
                && !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
                && self.draft.text().is_empty()
            {
                self.layer = Layer::Commands(CommandMenu::new(CommandOrigin::Slash));
                return false;
            }
            if composing && pressed && key.code == KeyCode::F(6) && !self.busy {
                self.open_memories(worker, true);
                return false;
            }
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
            let enqueue = composing && ctrl && key.code == KeyCode::Char('e');
            let send_on_enter = !composing
                || (!self.demo
                    && !key
                        .modifiers
                        .intersects(KeyModifiers::SHIFT | KeyModifiers::ALT));
            if pressed
                && (enqueue
                    || (ctrl && key.code == KeyCode::Char('s'))
                    || (key.code == KeyCode::Enter && send_on_enter))
            {
                self.submit(edit, worker, enqueue);
                return false;
            }
            let changed = match key.code {
                KeyCode::Enter if composing && !send_on_enter => {
                    value.insert("\n", edit_limit(&edit))
                }
                KeyCode::Char('n') if composing && ctrl => value.insert("\n", edit_limit(&edit)),
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
        if matches!(self.layer, Layer::Commands(_)) {
            let Layer::Commands(mut menu) = std::mem::replace(&mut self.layer, Layer::None) else {
                unreachable!()
            };
            let slash = menu.origin == CommandOrigin::Slash;
            match key.code {
                KeyCode::Backspace if slash && menu.query.is_empty() => {
                    self.layer = Layer::Edit(Edit::Task);
                    return false;
                }
                KeyCode::Backspace => {
                    if let Some((offset, _)) = menu.query.grapheme_indices(true).next_back() {
                        menu.query.truncate(offset);
                    }
                    menu.index = 0;
                }
                KeyCode::Char(c) if !ctrl && !c.is_control() => {
                    push_bounded(&mut menu.query, c.encode_utf8(&mut [0; 4]), 128);
                    menu.index = 0;
                }
                _ => {}
            }
            let commands = self.filtered_commands(&menu);
            if slash && commands.is_empty() && !menu.query.is_empty() {
                self.layer = Layer::Edit(Edit::Task);
                self.paste(&format!("/{}", menu.query));
                return false;
            }
            menu.index = menu.index.min(commands.len().saturating_sub(1));
            step(key.code, &mut menu.index, commands.len());
            if slash && pressed && key.code == KeyCode::Enter {
                self.layer = Layer::Edit(Edit::Task);
                self.paste(&format!("/{}", menu.query));
                return false;
            }
            let confirm = if slash { KeyCode::Tab } else { KeyCode::Enter };
            if pressed
                && key.code == confirm
                && let Some(command) = commands.get(menu.index)
            {
                match command.action {
                    CommandAction::Shortcut(key) => {
                        return self.overview_key(KeyEvent::new(key, KeyModifiers::NONE), worker);
                    }
                    CommandAction::NewConversation => {
                        if let Some(workspace) = &self.workspace
                            && !self.send(worker, Request::NewSession(workspace.id.clone(), None))
                        {
                            self.message = "Could not start a conversation. Try again.".into();
                            if menu.origin != CommandOrigin::Workbench {
                                self.layer = Layer::Edit(Edit::Task);
                            }
                        }
                        return false;
                    }
                    CommandAction::ModelProfiles => {
                        if self.send(worker, Request::ModelProfiles) {
                            self.layer = Layer::ModelPicker(model_picker::Picker::new(menu.origin));
                        } else {
                            self.layer = Layer::Commands(menu);
                        }
                        return false;
                    }
                    CommandAction::Help => {
                        self.layer = Layer::Commands(CommandMenu::new(CommandOrigin::Composer));
                        return false;
                    }
                }
            }
            self.layer = Layer::Commands(menu);
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
                                self.show_details = true;
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
            Layer::Recovery(id, scroll) => {
                scroll_key(key.code, scroll);
                if pressed
                    && let Some(detail) =
                        self.detail.as_ref().filter(|detail| detail.task.id == *id)
                {
                    if key.code == KeyCode::Char('y')
                        && let Some(disposition) =
                            detail.resume.as_ref().map(|status| status.disposition)
                        && matches!(
                            disposition,
                            TaskResumeDisposition::Ready
                                | TaskResumeDisposition::RetryUncertainModelStep
                        )
                    {
                        request = Some(Request::Resume(fliuno_protocol::TaskResumeParams {
                            task_id: id.clone(),
                            retry_uncertain_model_step: disposition
                                == TaskResumeDisposition::RetryUncertainModelStep,
                        }));
                        close = true;
                    } else if key.code == KeyCode::Char('o')
                        && detail.can_reconcile
                        && detail.review.is_none()
                    {
                        let state = detail
                            .task
                            .agent_run
                            .as_ref()
                            .and_then(|run| run.remote_task.as_ref())
                            .map(|remote| remote.state)
                            .filter(|state| state.is_terminal());
                        self.message.clear();
                        self.layer =
                            Layer::Reconcile(Box::new(ReconcileForm::new(id.clone(), state)));
                    }
                }
            }
            Layer::Diagnostics(_, scroll) => scroll_key(key.code, scroll),
            Layer::Memories(_) | Layer::Deliverables(_) => unreachable!(),
            Layer::Backup(_) => {
                let edit = match key.code {
                    KeyCode::Char('c') => Some(Edit::BackupCreate),
                    KeyCode::Char('v') => Some(Edit::BackupVerify),
                    KeyCode::Char('r') => Some(Edit::BackupRestoreSource),
                    _ => None,
                };
                if pressed
                    && !self.busy
                    && let Some(edit) = edit
                {
                    self.input.clear();
                    self.message.clear();
                    self.layer = Layer::Edit(edit);
                }
            }
            Layer::Reconcile(form) => match key.code {
                KeyCode::Tab if pressed => form.next_field(),
                KeyCode::Char('s') if ctrl && pressed => match form.params() {
                    Ok(params) => request = Some(Request::Reconcile(params)),
                    Err(message) => self.message = message.into(),
                },
                code if !ctrl && !key.modifiers.contains(KeyModifiers::ALT) => form.key(code),
                _ => {}
            },
            Layer::ConfirmCancel(id) => {
                if pressed && key.code == KeyCode::Char('y') {
                    request = Some(Request::Cancel(id.clone()));
                    close = true;
                }
            }
            Layer::None => return self.overview_key(key, worker),
            Layer::Inbox(..)
            | Layer::History(..)
            | Layer::Commands(_)
            | Layer::ModelPicker(_)
            | Layer::Edit(..) => {
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
                if options.mode == ContextMode::Isolated {
                    options.memory_selection = None;
                }
            }
            KeyCode::Char('m') if options.mode == ContextMode::Session => {
                options.disable_memory = !options.disable_memory;
                if options.disable_memory {
                    options.memory_selection = None;
                }
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
            self.layer = Layer::Commands(CommandMenu::new(CommandOrigin::Workbench));
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
            self.layer = Layer::Commands(CommandMenu::new(CommandOrigin::Workbench));
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
            self.layer = Layer::Commands(CommandMenu::new(CommandOrigin::Workbench));
            return false;
        }
        if key.code == KeyCode::Char('q') && pressed {
            return true;
        }
        if key.code == KeyCode::Char('r') && pressed && !self.online {
            self.send(worker, Request::Reconnect);
            return false;
        }
        if key.code == KeyCode::F(5) && pressed && !self.busy {
            if self.send(worker, Request::Diagnostics) {
                self.layer = Layer::Diagnostics(String::new(), 0);
            }
            return false;
        }
        if key.code == KeyCode::F(4) && pressed && !self.busy {
            self.layer = Layer::Backup(None);
            self.message.clear();
            return false;
        }
        if !self.online {
            return false;
        }
        let active = !self.busy && pressed;
        match key.code {
            KeyCode::Tab | KeyCode::Esc if pressed && self.show_details => {
                self.show_details = false;
                self.set_conversation_scroll(0);
                if !self.demo {
                    if let Some(latest) = self.tasks().first()
                        && self.selected.as_deref() != Some(&latest.id)
                    {
                        self.select_task(latest.id.clone());
                    }
                    self.follow = true;
                }
            }
            KeyCode::Tab if pressed => {
                self.show_details = true;
                self.set_conversation_scroll(0);
            }
            KeyCode::F(6) if active => self.open_memories(worker, false),
            KeyCode::F(7) if active => self.open_deliverables(worker),
            KeyCode::End => self.follow = true,
            KeyCode::Home => {
                self.follow = false;
                self.set_conversation_scroll(0);
            }
            KeyCode::Up | KeyCode::Down | KeyCode::PageUp | KeyCode::PageDown => {
                self.follow = false;
                let mut scroll = self.rendered_scroll.get();
                if key.code == KeyCode::PageUp
                    && scroll == 0
                    && active
                    && !self.show_details
                    && let Some(params) = self
                        .session
                        .as_ref()
                        .and_then(|session| self.transcript.older(&session.id))
                    && self.send(worker, Request::OlderTranscript(params))
                {
                    self.message.clear();
                    return false;
                }
                scroll_key(key.code, &mut scroll);
                self.set_conversation_scroll(scroll);
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
            KeyCode::Enter
                if active
                    && self.session.is_some()
                    && self.detail.as_ref().is_none_or(|detail| {
                        pending_approval_id(&detail.task).is_none()
                            && detail.task.pending_tool_input.is_none()
                    }) =>
            {
                self.layer = Layer::Edit(Edit::Task)
            }
            KeyCode::Char('n') if pressed && !self.start_pending && self.session.is_some() => {
                self.layer = Layer::Edit(Edit::Task);
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
        let mut commands = vec![
            ("Database backups", KeyCode::F(4)),
            ("Connection and configuration", KeyCode::F(5)),
        ];
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
            commands.push(("Workspace memories", KeyCode::F(6)));
            commands.push(("Workspace deliverables", KeyCode::F(7)));
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
                if detail.resume.is_some() || detail.can_reconcile {
                    commands.push(("Review task recovery", KeyCode::Char('r')));
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

    fn filtered_commands(&self, menu: &CommandMenu) -> Vec<Command> {
        let mut commands = Vec::new();
        if self.online && self.workspace.is_some() && !self.busy {
            commands.push(Command {
                label: "Start a new conversation",
                alias: Some("new"),
                action: CommandAction::NewConversation,
            });
        }
        if self.model_profiles && !self.busy {
            commands.push(Command {
                label: "Switch model",
                alias: Some("model"),
                action: CommandAction::ModelProfiles,
            });
        }
        if menu.origin == CommandOrigin::Slash {
            commands.push(Command {
                label: "Show all commands",
                alias: Some("help"),
                action: CommandAction::Help,
            });
        }
        commands.extend(
            self.commands()
                .into_iter()
                .map(|(label, key)| Command::shortcut(label, key)),
        );
        let query = menu.query.to_lowercase();
        commands.retain(|command| command.matches(menu.origin, &query));
        commands
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
                    self.show_details = true;
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
            KeyCode::Char('r') if detail.resume.is_some() || detail.can_reconcile => {
                self.layer = Layer::Recovery(task.id.clone(), 0);
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
        if matches!(edit, Edit::BackupRestoreSource) {
            self.layer = Layer::Edit(Edit::BackupRestoreDestination(value.into()));
            self.input.clear();
            return;
        }
        let backup = matches!(
            edit,
            Edit::BackupCreate | Edit::BackupVerify | Edit::BackupRestoreDestination(_)
        );
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
                Request::NewSession(workspace.id.clone(), Some(value))
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
            Edit::BackupCreate => Request::Backup(BackupRequest::Create(value.into())),
            Edit::BackupVerify => Request::Backup(BackupRequest::Verify(value.into())),
            Edit::BackupRestoreSource => unreachable!(),
            Edit::BackupRestoreDestination(source) => Request::Backup(BackupRequest::Restore {
                source,
                destination: value.into(),
            }),
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
            } else if backup {
                self.layer = Layer::Backup(None);
            } else if !fork {
                self.layer = Layer::None;
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
    if let Some(picks) = &preview.options.memory_selection {
        lines.push(format!("Pinned memory revisions: {}", picks.len()));
    } else if !preview.options.disable_memory {
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

fn model_label(model: String) -> Option<String> {
    let label = clean(&model).replace('\n', " ").trim().to_owned();
    (!label.is_empty()).then_some(label)
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
