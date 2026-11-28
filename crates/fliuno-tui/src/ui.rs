mod answer;
mod approval;
mod commands;
mod deliverables;
mod draft_budget;
mod language;
mod memories;
mod model_connection;
mod model_edit;
mod model_picker;
mod palette;
mod panels;
mod project_activity;
mod project_checks;
mod project_guide;
pub(crate) mod setup;
mod setup_catalog;
mod supplier_settings;
mod transcript;
mod workbench;

use commands::{Command, CommandAction, CommandMenu, CommandOrigin};
use language::{Language, LanguageText};
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
    Message, ObjectPage, PolicyReadRule, PolicyReadRuleCreateParams, Session, SessionHistoryRole,
    Task, TaskResumeDisposition, TaskStatus, TaskSummary, WorkActionKind, WorkSnapshot, Workspace,
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
    ContextDetail, ContextMessage, ContextMode, ContextOptions, DraftContextParams, EventKind,
    SessionForkParams, SessionListOrder, SessionListParams, TaskContext, TaskContextAttribution,
    TaskQueue, TaskQueueItemParams, TaskQueueState, TaskTimelinePage, TaskTimelineParams,
    ToolScope,
};

const DRAFT_LIMIT: usize = 48 * 1024;

enum Layer {
    Commands(CommandMenu),
    Language(language::Panel),
    ModelPicker(Box<model_picker::Picker>),
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
    Context {
        text: String,
        scroll: u16,
        rejected_task: Option<String>,
    },
    DraftContext {
        preview: Option<Box<crate::service::DraftContextView>>,
        scroll: u16,
    },
    Approval {
        task: String,
        id: String,
        review: Box<approval::Review>,
        remember: Option<Box<PolicyReadRuleCreateParams>>,
    },
    ReadRules(Option<Vec<PolicyReadRule>>, usize),
    ProjectChecks(Box<project_checks::Panel>),
    ProjectGuide(Box<project_guide::Panel>),
    ProjectReview {
        return_to_draft: bool,
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

struct DeferredSend {
    session: String,
    key: String,
}

pub struct App {
    demo: bool,
    palette: Palette,
    language: Language,
    config_path: Option<PathBuf>,
    online: bool,
    model: Option<String>,
    model_profiles: bool,
    supplier_settings: bool,
    supplier_catalog: bool,
    read_rules: bool,
    project_checks: bool,
    model_tool_scope: bool,
    project_guide: Option<fliuno_protocol::WorkspaceProjectGuide>,
    busy: bool,
    next: Option<Request>,
    reopening: Option<String>,
    deferred_send: Option<DeferredSend>,
    deferred_approval: Option<(String, String)>,
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
    scroll: Cell<usize>,
    scroll_anchor: Cell<Option<transcript::Anchor>>,
    transcript: Transcript,
    rendered_scroll: Cell<usize>,
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
    pub fn new(demo: bool, theme: Theme, language: Language, config_path: Option<PathBuf>) -> Self {
        Self {
            demo,
            palette: Palette::new(theme),
            language,
            config_path,
            online: false,
            model: None,
            model_profiles: false,
            supplier_settings: false,
            supplier_catalog: false,
            read_rules: false,
            project_checks: false,
            model_tool_scope: false,
            project_guide: None,
            busy: true,
            next: None,
            reopening: None,
            deferred_send: None,
            deferred_approval: None,
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
        if self.busy {
            return false;
        }
        let reopening = match &request {
            Request::SelectSession(id) => Some(id.clone()),
            // The snapshot can schedule a detail read before the composer is ready.
            Request::Task(_) => self.reopening.clone(),
            _ => None,
        };
        if !worker.send(request) {
            return false;
        }
        self.reopening = reopening;
        self.busy = true;
        true
    }

    fn sync(&mut self, worker: &Worker) {
        if self.busy {
            return;
        }
        if let Some(next) = self.next.take() {
            if !matches!(next, Request::Task(_)) {
                self.cancel_deferred_send();
            }
            if !self.send(worker, next) {
                self.reopening = None;
                self.deferred_send = None;
                self.message = "Connection worker unavailable".into();
            }
            return;
        }
        self.reopening = None;
        if let Some((task, approval)) = self.deferred_approval.take()
            && self.online
            && matches!(self.layer, Layer::None)
            && self.selected.as_deref() == Some(task.as_str())
            && self.detail.as_ref().is_some_and(|detail| {
                detail.task.id == task && pending_approval_id(&detail.task) == Some(&approval)
            })
            && self.send(worker, Request::Approval(task, approval))
        {
            return;
        }
        if let Some(intent) = self.deferred_send.take()
            && self.online
            && matches!(self.layer, Layer::Edit(Edit::Task))
            && self
                .session
                .as_ref()
                .is_some_and(|session| session.id == intent.session)
            && self.key == intent.key
        {
            if !self.can_compose() {
                self.message = "Review the pending request before sending. Draft kept.".into();
                return;
            }
            let text = self.draft.text().trim().to_owned();
            self.start_draft(worker, intent.session, text, false);
        }
    }

    fn can_compose(&self) -> bool {
        self.detail.as_ref().is_none_or(|detail| {
            pending_approval_id(&detail.task).is_none() && detail.task.pending_tool_input.is_none()
        })
    }

    fn cancel_deferred_send(&mut self) -> bool {
        if self.deferred_send.take().is_some() {
            self.message = "Send cancelled. Draft kept.".into();
            true
        } else {
            false
        }
    }

    fn start_draft(&mut self, worker: &Worker, session: String, text: String, enqueue: bool) {
        let request = Request::Start {
            session,
            text,
            key: self.key.clone(),
            input: self.input_artifact.clone(),
            plan_only: self.plan_only,
            context: Box::new(self.project_context_options()),
            enqueue,
        };
        if self.send(worker, request) {
            self.start_pending = true;
        } else {
            self.message = "Connection worker unavailable. Draft kept.".into();
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
        self.deferred_approval = None;
        self.cancel_deferred_send();
        self.reopening = None;
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

    fn set_conversation_scroll(&self, row: usize) {
        self.scroll.set(row);
        self.scroll_anchor.set(None);
    }

    fn select_task(&mut self, id: String) {
        self.deferred_approval = None;
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
                supplier_settings,
                supplier_catalog,
                read_rules,
                project_checks,
                model_tool_scope,
                project_guide,
            } => {
                self.online = true;
                self.model = model.and_then(model_label);
                self.model_profiles = model_profiles;
                self.supplier_settings = supplier_settings;
                self.supplier_catalog = supplier_catalog;
                self.read_rules = read_rules;
                self.project_checks = project_checks;
                self.model_tool_scope = model_tool_scope;
                self.project_guide = project_guide;
                self.activity.clear();
                self.message.clear();
                let id = self.workspace.get_or_insert(workspace).id.clone();
                self.next = Some(match &self.session {
                    Some(session) => Request::SelectSession(session.id.clone()),
                    None => sessions(id, 0, false),
                });
            }
            Update::ProjectChecks(checks) => self.install_project_checks(checks),
            Update::ProjectGuide(preview) => self.install_project_guide(*preview),
            Update::Workspace(workspace, project_guide) => {
                self.project_guide = project_guide;
                self.cancel_deferred_send();
                self.reopening = None;
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
                if (matches!(self.layer, Layer::None) || self.reopening.as_ref() == Some(&session))
                    && self.selected.is_none()
                    && let Some(task) = self.tasks().first()
                {
                    self.select_task(task.id.clone());
                    self.follow = !self.demo;
                } else if matches!(self.layer, Layer::None) {
                    if previous != Some(current)
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
            Update::Approval(task, id, details, remember) => {
                if self.selected.as_deref() == Some(task.as_str())
                    && matches!(self.layer, Layer::None)
                    && let Some(review) = approval::review(
                        &task,
                        &id,
                        self.workspace
                            .as_ref()
                            .map(|root| display_path(&root.root))
                            .as_deref()
                            .unwrap_or(""),
                        *details,
                        self.language,
                        remember.is_some(),
                    )
                {
                    self.message.clear();
                    self.layer = Layer::Approval {
                        task,
                        id,
                        review: Box::new(review),
                        remember,
                    };
                }
            }
            Update::ReadRules(workspace, rules) => {
                if self
                    .workspace
                    .as_ref()
                    .is_some_and(|root| root.id == workspace)
                    && let Layer::ReadRules(current, index) = &mut self.layer
                {
                    *index = (*index).min(rules.len().saturating_sub(1));
                    *current = Some(rules);
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
                    && matches!(self.layer, Layer::Context { .. })
                {
                    let first_request = matches!(
                        attribution
                            .latest_budget_rejection
                            .as_ref()
                            .map(|event| &event.data),
                        Some(EventKind::ModelBudgetRejected {
                            step: 1,
                            summarization: false,
                            ..
                        })
                    ) && attribution.model_steps == 1;
                    self.layer = Layer::Context {
                        text: format_context_attribution(&attribution, self.language),
                        scroll: 0,
                        rejected_task: first_request.then_some(attribution.task_id),
                    };
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
                self.draft_context.tool_scope = ToolScope::All;
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
                self.draft_context.tool_scope = ToolScope::All;
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
            Update::Notice(message) => self.message = message,
            Update::Error(message) => {
                self.deferred_send = None;
                self.reopening = None;
                self.message = message;
            }
            Update::Offline(message) => {
                self.deferred_approval = None;
                self.deferred_send = None;
                self.reopening = None;
                self.online = false;
                self.model = None;
                self.model_profiles = false;
                self.supplier_settings = false;
                self.supplier_catalog = false;
                self.read_rules = false;
                self.project_checks = false;
                self.model_tool_scope = false;
                self.project_guide = None;
                self.activity.clear();
                self.layer = Layer::None;
                self.next = None;
                self.message = message;
            }
            Update::Activity(previews) => self.activity = previews,
            Update::ModelProfiles(profiles, default, features) => {
                if let Layer::ModelPicker(picker) = &mut self.layer {
                    picker.install(profiles, default, features);
                }
            }
            Update::SupplierSettings(snapshot) => {
                if let Layer::ModelPicker(picker) = &mut self.layer {
                    picker.supplier_snapshot(*snapshot);
                }
            }
            Update::SupplierCatalog(preview) => {
                if let Layer::ModelPicker(picker) = &mut self.layer {
                    picker.supplier_catalog(preview);
                }
            }
            Update::SupplierSettingsSaved => {
                if let Layer::ModelPicker(picker) = &mut self.layer {
                    picker.supplier_saved();
                }
            }
            Update::ModelConnectionOpened(target, view) => {
                if let Layer::ModelPicker(picker) = &mut self.layer {
                    picker.connection_opened(target, view);
                }
            }
            Update::ModelConnectionPreviewed(preview) => {
                if let Layer::ModelPicker(picker) = &mut self.layer {
                    picker.connection_previewed(preview);
                }
            }
            Update::ModelConnectionSaved(entry) => {
                if let Layer::ModelPicker(picker) = &mut self.layer {
                    picker.connection_saved(entry);
                }
            }
            Update::ModelEditOpened(view) => {
                if let Layer::ModelPicker(picker) = &mut self.layer {
                    picker.edit_opened(view);
                }
            }
            Update::ModelEditSaved(view) => {
                if let Layer::ModelPicker(picker) = &mut self.layer {
                    picker.edit_saved(view);
                }
            }
            Update::ModelCatalogPage(page) => {
                if let Layer::ModelPicker(picker) = &mut self.layer {
                    picker.catalog_page(page);
                }
            }
            Update::ModelDefaultSaved(default) => {
                if let Layer::ModelPicker(picker) = &mut self.layer {
                    picker.saved(default);
                }
            }
            Update::ModelChecked { profile, check } => {
                if let Layer::ModelPicker(picker) = &mut self.layer {
                    picker.checked(profile, check);
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
                    picker.failed(clean(&error));
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
                self.cancel_deferred_send();
            }
        } else if let Layer::ModelPicker(picker) = &mut self.layer {
            picker.paste(value, self.busy);
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
            self.cancel_deferred_send();
            return true;
        }
        if key.code == KeyCode::Esc
            && pressed
            && (self.cancel_deferred_send() || self.deferred_approval.take().is_some())
        {
            return false;
        }
        if matches!(self.layer, Layer::ProjectChecks(_)) {
            self.project_checks_key(key, worker);
            return false;
        }
        if matches!(self.layer, Layer::ProjectGuide(_)) {
            self.project_guide_key(key, worker);
            return false;
        }
        if matches!(self.layer, Layer::Language(_)) {
            self.language_key(key);
            return false;
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
        if pressed
            && key.code == KeyCode::Char('e')
            && let Layer::Context {
                rejected_task: Some(id),
                ..
            } = &self.layer
        {
            if let Some(text) = self.rejected_draft_text(id).map(str::to_owned)
                && self.draft.insert(&text, DRAFT_LIMIT)
            {
                self.key = uuid::Uuid::new_v4().to_string();
                self.open_draft_context(worker);
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
        if key.code == KeyCode::Esc
            && matches!(
                self.layer,
                Layer::ProjectReview {
                    return_to_draft: true,
                    ..
                }
            )
        {
            self.layer = Layer::Edit(Edit::Task);
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
                self.cancel_deferred_send();
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
            if matches!(edit, Edit::Task) && pressed && key.code == KeyCode::F(3) {
                self.cancel_deferred_send();
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
                self.cancel_deferred_send();
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
                            self.layer = Layer::ModelPicker(Box::new(model_picker::Picker::new(
                                menu.origin,
                            )));
                        } else {
                            self.layer = Layer::Commands(menu);
                        }
                        return false;
                    }
                    CommandAction::SupplierSettings => {
                        if self.send(worker, Request::SupplierSettings) {
                            self.layer =
                                Layer::ModelPicker(Box::new(model_picker::Picker::settings(
                                    menu.origin,
                                    self.supplier_catalog,
                                    self.language,
                                )));
                        } else {
                            self.layer = Layer::Commands(menu);
                        }
                        return false;
                    }
                    CommandAction::Language => {
                        self.layer =
                            Layer::Language(language::Panel::new(menu.origin, self.language));
                        return false;
                    }
                    CommandAction::ReadRules => {
                        if self.send(worker, Request::ReadRules) {
                            self.message.clear();
                            self.layer = Layer::ReadRules(None, 0);
                        } else {
                            self.layer = Layer::Commands(menu);
                        }
                        return false;
                    }
                    CommandAction::TogglePlan => {
                        self.toggle_plan_mode();
                        if menu.origin != CommandOrigin::Workbench {
                            self.layer = Layer::Edit(Edit::Task);
                        }
                        return false;
                    }
                    CommandAction::ProjectChecks => {
                        if !self.open_project_checks(menu.origin, worker) {
                            self.layer = Layer::Commands(menu);
                        }
                        return false;
                    }
                    CommandAction::ProjectGuide => {
                        if !self.open_project_guide(menu.origin, worker) {
                            self.layer = Layer::Commands(menu);
                        }
                        return false;
                    }
                    CommandAction::ProjectReview => {
                        self.open_project_review(menu.origin != CommandOrigin::Workbench);
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
            Layer::Context { scroll, .. } | Layer::TimelineEvent { scroll, .. } => {
                scroll_key(key.code, scroll)
            }
            Layer::ProjectReview { scroll, .. } => {
                if key.code == KeyCode::Home {
                    *scroll = 0;
                } else {
                    scroll_key(key.code, scroll);
                }
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
                review,
                remember,
            } => {
                if !ctrl {
                    review.navigate(key.code, pressed);
                }
                if pressed && !ctrl {
                    request = match key.code {
                        KeyCode::Char('y') if review.allow_once => Some(Request::Decide(
                            task.clone(),
                            id.clone(),
                            ApprovalDecision::AllowOnce,
                        )),
                        KeyCode::Char('n') => Some(Request::Decide(
                            task.clone(),
                            id.clone(),
                            ApprovalDecision::Deny,
                        )),
                        KeyCode::Char('r') if review.allow_once => remember.as_ref().map(|scope| {
                            Request::RememberRead(task.clone(), id.clone(), scope.clone())
                        }),
                        _ => None,
                    };
                    close = request.is_some();
                }
            }
            Layer::ReadRules(rules, index) => {
                if let Some(rules) = rules {
                    step(key.code, index, rules.len());
                    if pressed
                        && key.code == KeyCode::Delete
                        && let Some(rule) = rules.get(*index)
                    {
                        request = Some(Request::RevokeRead(rule.id.clone()));
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
            | Layer::Language(_)
            | Layer::ProjectChecks(_)
            | Layer::ProjectGuide(_)
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

    // Only copy a text-only first attempt with positive evidence of pre-dispatch
    // rejection. Do not restore a stale authorization, guide revision, or history pin.
    fn rejected_draft_text(&self, id: &str) -> Option<&str> {
        if !self.online
            || self.start_pending
            || self.input_artifact.is_some()
            || !self.draft.text().is_empty()
            || !self.can_compose()
        {
            return None;
        }
        let task = &self.detail.as_ref()?.task;
        if self.selected.as_deref() != Some(id)
            || task.id != id
            || self.session.as_ref()?.id != task.session_id
            || task.backend != fliuno_protocol::TaskBackend::Model
            || task.status != TaskStatus::Failed
            || task.model_steps != 1
            || task.model_outcome_unknown
            || task.in_flight_tool.is_some()
        {
            return None;
        }
        match task.messages.as_slice() {
            [Message::User { text }] if !text.is_empty() && text.len() <= DRAFT_LIMIT => Some(text),
            _ => None,
        }
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
            let mut options = self.project_context_options();
            options.plan_only = self.plan_only;
            let params = DraftContextParams {
                session_id: session.id.clone(),
                input: self.draft.text().trim().to_owned(),
                options,
            };
            if self.send(worker, Request::DraftContext(Box::new(params))) {
                self.message.clear();
            } else {
                self.message = self
                    .language
                    .text("Press R to refresh when ready.", "就绪后按 R 刷新。")
                    .into();
            }
        }
    }

    fn draft_context_key(&mut self, key: KeyEvent, worker: &Worker) {
        let Layer::DraftContext { preview, scroll } = &mut self.layer else {
            return;
        };
        if key.code == KeyCode::Home {
            *scroll = 0;
        } else {
            scroll_key(key.code, scroll);
        }
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
            KeyCode::Char('t') if self.model_tool_scope => {
                options.tool_scope = match options.tool_scope {
                    ToolScope::All => ToolScope::Project,
                    ToolScope::Project => ToolScope::None,
                    ToolScope::None => ToolScope::All,
                };
            }
            KeyCode::Char('p') if options.mode == ContextMode::Session => {
                options.through_sequence = match options.through_sequence {
                    Some(_) => None,
                    None => preview.as_ref().map(|value| value.history.through_sequence),
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
            KeyCode::Char('d') if active && !self.demo => self.open_project_review(false),
            KeyCode::End => self.follow = true,
            KeyCode::Home => {
                self.follow = false;
                self.set_conversation_scroll(0);
            }
            KeyCode::Up | KeyCode::Down | KeyCode::PageUp | KeyCode::PageDown => {
                self.follow = false;
                let scroll = self.rendered_scroll.get();
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
                self.set_conversation_scroll(
                    scroll.saturating_add_signed(isize::from(scroll_delta(key.code))),
                );
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
            KeyCode::F(3) if pressed => self.open_draft_context(worker),
            KeyCode::Char('H') if active => {
                self.input.clear();
                self.layer = Layer::Edit(Edit::HistorySearch);
            }
            KeyCode::Enter | KeyCode::Char('a')
                if pressed
                    && self.busy
                    && self
                        .detail
                        .as_ref()
                        .is_some_and(|detail| pending_approval_id(&detail.task).is_some()) =>
            {
                // A background detail refresh can be in progress while Review is visible.
                // Only remember opening the exact pending review, never an approval decision.
                if let Some(detail) = &self.detail
                    && let Some(approval) = pending_approval_id(&detail.task)
                {
                    self.deferred_approval = Some((detail.task.id.clone(), approval.to_owned()));
                }
            }
            KeyCode::Enter
                if (active || (!self.demo && pressed && self.reopening.is_some()))
                    && self.session.is_some()
                    && self.can_compose() =>
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

    fn commands(&self) -> Vec<((&'static str, &'static str), KeyCode)> {
        let mut commands = vec![
            (("Database backups", "数据库备份"), KeyCode::F(4)),
            (
                ("Connection and configuration", "连接与配置"),
                KeyCode::F(5),
            ),
        ];
        if self.online {
            commands.push((
                if self.session.is_some() {
                    ("New task", "新建任务")
                } else {
                    ("New session", "新建会话")
                },
                KeyCode::Char('n'),
            ));
            commands.push((("Workspaces", "工作区列表"), KeyCode::Char('w')));
            commands.push((("Sessions", "会话列表"), KeyCode::Char('s')));
            commands.push((("Workspace memories", "工作区记忆"), KeyCode::F(6)));
            commands.push((("Workspace deliverables", "工作区交付物"), KeyCode::F(7)));
            if self.session.is_some() {
                commands.push((("Browse session tasks", "浏览会话任务"), KeyCode::Char('L')));
                commands.push((("Queued prompts", "排队的提示"), KeyCode::Char('Q')));
                commands.push((
                    ("Branch current session", "从当前会话分支"),
                    KeyCode::Char('b'),
                ));
                commands.push((("Review draft context", "查看草稿上下文"), KeyCode::F(3)));
            }
            commands.push((("Workspace inbox", "工作区待办"), KeyCode::Char('I')));
            commands.push((
                ("Search workspace history", "查找工作区历史"),
                KeyCode::Char('H'),
            ));
            if let Some(detail) = &self.detail {
                let task = &detail.task;
                if !task.status.is_terminal() {
                    if task.status != TaskStatus::Interrupted {
                        commands.push((("Add instruction", "补充指令"), KeyCode::Char('i')));
                    }
                    commands.push((("Cancel task", "取消任务"), KeyCode::Char('x')));
                }
                if detail.resume.is_some() || detail.can_reconcile {
                    commands.push((("Review task recovery", "查看任务恢复"), KeyCode::Char('r')));
                }
                commands.push((("Task timeline", "任务时间线"), KeyCode::Char('t')));
                commands.push((
                    ("Context and memory usage", "上下文与记忆用量"),
                    KeyCode::Char('c'),
                ));
                if task.status == TaskStatus::Completed {
                    commands.push((
                        ("Branch from completed task", "从已完成任务分支"),
                        KeyCode::Char('B'),
                    ));
                }
                if Self::can_implement(detail) {
                    commands.push((("Draft implementation", "生成实施草稿"), KeyCode::Char('P')));
                }
                if !detail.artifacts.is_empty() {
                    commands.push((("Open artifacts", "打开交付物"), KeyCode::Char('v')));
                }
            }
        } else {
            commands.push((("Reconnect", "重新连接"), KeyCode::Char('r')));
        }
        commands.push((("Quit", "退出"), KeyCode::Char('q')));
        commands
    }

    fn filtered_commands(&self, menu: &CommandMenu) -> Vec<Command> {
        let mut commands = Vec::new();
        if menu.origin != CommandOrigin::Slash
            && self.online
            && !self.busy
            && let Some(detail) = &self.detail
        {
            if pending_approval_id(&detail.task).is_some() {
                commands.push(
                    Command::shortcut(
                        self.language
                            .text("Review pending operation", "审阅待批准的操作"),
                        KeyCode::Char('a'),
                    )
                    .with_search_label("Review pending operation"),
                );
            } else if detail
                .task
                .pending_tool_input
                .as_ref()
                .is_some_and(|input| input.requests["kind"] == "clarification")
            {
                commands.push(
                    Command::shortcut(
                        self.language.text("Answer clarification", "回答澄清问题"),
                        KeyCode::Char('a'),
                    )
                    .with_search_label("Answer clarification"),
                );
            }
        }
        if self.session.is_some() && !self.demo {
            commands.push(Command {
                label: self
                    .language
                    .text("Review changes and checks", "查看改动与检查"),
                alias: Some("changes"),
                action: CommandAction::ProjectReview,
                search_label: None,
            });
        }
        if self.online && self.session.is_some() && !self.busy {
            commands.push(Command {
                label: if self.plan_only {
                    self.language
                        .text("Switch to execution mode", "切换到执行模式")
                } else {
                    self.language.text("Switch to plan mode", "切换到计划模式")
                },
                alias: Some("plan"),
                action: CommandAction::TogglePlan,
                search_label: None,
            });
        }
        if self.online && self.workspace.is_some() && !self.busy {
            commands.push(Command {
                label: self.language.text("Start a new conversation", "开始新对话"),
                alias: Some("new"),
                action: CommandAction::NewConversation,
                search_label: None,
            });
        }
        if self.model_profiles && !self.busy {
            commands.push(Command {
                label: self.language.text("Switch model", "切换模型"),
                alias: Some("model"),
                action: CommandAction::ModelProfiles,
                search_label: None,
            });
        }
        if self.supplier_settings && !self.busy {
            commands.push(Command {
                label: self
                    .language
                    .text("Model settings and suppliers", "模型和供应商设置"),
                alias: Some("settings"),
                action: CommandAction::SupplierSettings,
                search_label: None,
            });
        }
        commands.push(Command {
            label: self
                .language
                .text("Language · 简体中文", "界面语言 · English"),
            alias: Some("language"),
            action: CommandAction::Language,
            search_label: None,
        });
        if self.online && self.project_checks && self.workspace.is_some() && !self.busy {
            commands.push(Command {
                label: self.language.text("List project checks", "查看项目检查"),
                alias: Some("checks"),
                action: CommandAction::ProjectChecks,
                search_label: Some("List project checks"),
            });
        }
        if self.online && self.project_guide.is_some() && self.workspace.is_some() && !self.busy {
            commands.push(Command {
                label: self.language.text("Review project guide", "审阅项目指南"),
                alias: Some("guide"),
                action: CommandAction::ProjectGuide,
                search_label: Some("Review project guide"),
            });
        }
        if self.online && self.read_rules && self.workspace.is_some() && !self.busy {
            commands.push(Command {
                label: self
                    .language
                    .text("Manage read permissions", "管理读取权限"),
                alias: Some("permissions"),
                action: CommandAction::ReadRules,
                search_label: None,
            });
        }
        if menu.origin == CommandOrigin::Slash {
            commands.push(Command {
                label: self.language.text("Show all commands", "查看全部命令"),
                alias: Some("help"),
                action: CommandAction::Help,
                search_label: None,
            });
        }
        commands.extend(self.commands().into_iter().map(|((en, zh), key)| {
            Command::shortcut(self.language.text(en, zh), key).with_search_label(en)
        }));
        let query = menu.query.to_lowercase();
        commands.retain(|command| command.matches(menu.origin, &query));
        if !query.is_empty() {
            // An exact slash name wins over words in unrelated command captions.
            commands.sort_by_key(|command| command.alias != Some(query.trim_start_matches('/')));
        }
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
                    self.layer = Layer::Context {
                        text: String::new(),
                        scroll: 0,
                        rejected_task: None,
                    };
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
        if matches!(edit, Edit::Task) {
            let Some(session) = &self.session else { return };
            if !self.busy {
                self.start_draft(worker, session.id.clone(), value.to_owned(), enqueue);
                return;
            }
            if !enqueue && !self.demo && self.reopening.as_ref() == Some(&session.id) {
                self.deferred_send.get_or_insert_with(|| DeferredSend {
                    session: session.id.clone(),
                    key: self.key.clone(),
                });
                return;
            }
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
        let history_search = matches!(edit, Edit::HistorySearch);
        let session_browse = matches!(edit, Edit::SessionSearch | Edit::RenameSession(_));
        let fork = matches!(edit, Edit::Fork(_));
        let history_query = history_search.then(|| value.clone());
        let request = match edit {
            Edit::Task => unreachable!(),
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
            if history_search {
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
    lang: Language,
) -> String {
    let Some(preview) = preview else {
        return if busy {
            lang.text("Checking draft context…", "正在检查草稿上下文…")
        } else {
            message
        }
        .to_owned();
    };
    if preview.options.mode == ContextMode::Isolated {
        return lang
            .text(
                "This prompt starts without session history or workspace memory.",
                "本轮不使用会话历史或工作区记忆。",
            )
            .into();
    }
    let mut lines = vec![format!(
        "{} {} · {} {} · {} #{}",
        preview.turns.len(),
        lang.text("completed turns", "条历史"),
        preview.omitted_turns,
        lang.text("omitted", "条省略"),
        lang.text("through", "截至"),
        preview.through_sequence,
    )];
    if let Some(picks) = &preview.options.memory_selection {
        lines.push(format!(
            "{}: {}",
            lang.text("Pinned memory revisions", "已固定的记忆版本"),
            picks.len()
        ));
    } else if !preview.options.disable_memory {
        lines.push(
            lang.text(
                "Automatic memory is selected when the task starts.",
                "开始任务时自动选择记忆。",
            )
            .into(),
        );
    }
    lines.push(String::new());
    for turn in preview.turns.iter().rev() {
        let prompt = turn.messages.iter().find_map(|message| match message {
            ContextMessage::User { text } => Some(text),
            _ => None,
        });
        let title = prompt.map_or_else(
            || lang.text("Completed task", "已完成任务").to_owned(),
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

fn format_context_attribution(report: &TaskContextAttribution, lang: Language) -> String {
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
    if let Some(guide) = &report.project_guide {
        lines.push(format!(
            "Project guide: {} · {} B\nSHA-256: {}",
            clean(&guide.path),
            guide.bytes,
            clean(&guide.sha256)
        ));
    }
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
        use fliuno_protocol::ModelBudgetLimit;
        let (name, unit) = match report.kind {
            ModelBudgetLimit::LocalJsonBytes => {
                (lang.text("Local input limit", "本地输入上限"), "B")
            }
            ModelBudgetLimit::ProviderInputTokens => (
                lang.text("Provider input limit", "供应商输入上限"),
                "tokens",
            ),
            ModelBudgetLimit::GenerationRequestBytes => {
                (lang.text("Generation request limit", "生成请求上限"), "B")
            }
            ModelBudgetLimit::CountRequestBytes => {
                (lang.text("Count request limit", "计数请求上限"), "B")
            }
        };
        let minimum = if report.observed_is_lower_bound {
            "≥"
        } else {
            ""
        };
        lines.splice(
            0..0,
            [
                format!(
                    "{name}: {minimum}{} / {} {unit}",
                    report.observed, report.limit
                ),
                lang.text("This model step was not sent.", "该模型步骤未发出。")
                    .into(),
                String::new(),
            ],
        );
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

fn scroll_delta(key: KeyCode) -> i16 {
    match key {
        KeyCode::Up => -1,
        KeyCode::Down => 1,
        KeyCode::PageUp => -10,
        KeyCode::PageDown => 10,
        _ => 0,
    }
}

fn scroll_key(key: KeyCode, scroll: &mut u16) {
    *scroll = scroll.saturating_add_signed(scroll_delta(key));
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

/// Keep exact edit reviews legible without hiding changes to line endings or tabs.
fn exact_edit_text(text: &str) -> String {
    use std::fmt::Write as _;
    let mut visible = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '\n' => visible.push_str("\\n\n"),
            '\r' | '\t' | '\\' => {
                let _ = write!(visible, "{}", c.escape_default());
            }
            c if c.is_control()
                || matches!(c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}') =>
            {
                let _ = write!(visible, "{}", c.escape_unicode());
            }
            _ => visible.push(c),
        }
    }
    visible
}

fn project_check_command(plan: &serde_json::Value) -> Option<String> {
    let mut words = Vec::new();
    words.push(plan["program"].as_str()?);
    for arg in plan["args"].as_array()? {
        words.push(arg.as_str()?);
    }
    // Delimit argv without implying that this text is run by a shell.
    Some(
        words
            .into_iter()
            .map(|word| {
                if !word.is_empty()
                    && word
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || "._/-".contains(c))
                {
                    word.to_owned()
                } else {
                    serde_json::to_string(word).expect("serializing a string is infallible")
                }
            })
            .collect::<Vec<_>>()
            .join(" "),
    )
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
    fn opens_the_same_pending_approval_after_refresh_and_drops_stale_or_cancelled_open() {
        let (worker, requests) = Worker::request_fixture();
        let mut app = App::new(false, Theme::Auto, Language::English, None);
        app.online = true;
        app.selected = Some("task".into());
        app.detail = Some(Box::new(Detail {
            task: serde_json::from_value(serde_json::json!({
                "id":"task", "sessionId":"session", "status":"waiting_approval", "messages":[], "modelSteps":1,
                "pendingApproval":{"id":"pending","taskId":"task","call":{"id":"call","name":"workspace_patch_text","arguments":{}}}
            })).unwrap(),
            plan: None, artifacts: Vec::new(), resume: None, review: None, can_reconcile: false, usage: None,
        }));
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        for _ in 0..2 {
            app.key(enter, &worker);
            assert!(requests.try_recv().is_err());
        }
        app.update(Update::Finished);
        app.sync(&worker);
        assert!(
            matches!(requests.try_recv().unwrap(), Request::Approval(task, approval) if task == "task" && approval == "pending")
        );
        assert!(requests.try_recv().is_err());

        app.key(enter, &worker);
        app.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &worker);
        app.update(Update::Finished);
        app.sync(&worker);
        assert!(requests.try_recv().is_err());

        app.busy = true;
        app.key(enter, &worker);
        app.detail
            .as_mut()
            .unwrap()
            .task
            .pending_approval
            .as_mut()
            .unwrap()
            .id = "new-approval".into();
        app.update(Update::Finished);
        app.sync(&worker);
        assert!(requests.try_recv().is_err());
    }

    #[test]
    fn stale_browse_cannot_change_the_current_session() {
        let mut app = App::new(true, Theme::Auto, Language::English, None);
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
        let mut app = App::new(true, Theme::Auto, Language::English, None);
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
            let mut app = App::new(true, theme, Language::English, None);
            app.layer = Layer::Approval {
                task: "task".into(),
                id: "approval".into(),
                review: Box::new(approval::Review::new(
                    "Operation with many arguments\n".repeat(40),
                    String::new(),
                    true,
                )),
                remember: None,
            };
            let mut loading = Terminal::new(TestBackend::new(48, 18)).unwrap();
            let frame = loading.draw(|frame| app.draw(frame)).unwrap();
            let text: String = frame
                .buffer
                .content()
                .iter()
                .map(|cell| cell.symbol())
                .collect();
            assert!(text.contains("Loading review…"));
            assert!(!text.contains("y Allow once"));
            app.update(Update::Finished);
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
        assert!(
            approval::review(
                "task",
                "different",
                "root",
                details.clone(),
                Language::English,
                false,
            )
            .is_none()
        );
        let review = approval::review(
            "task",
            "approval",
            "root",
            details,
            Language::English,
            false,
        )
        .unwrap();
        assert!(review.summary().contains("External Agent permission"));
        assert!(review.summary().contains("Read report"));
        assert!(!review.allow_once);

        approval.allow_once_available = true;
        approval.source = fliuno_protocol::AgentApprovalSource::HostTool;
        let details = ApprovalDetails::HostTool { approval };
        let review = approval::review(
            "task",
            "approval",
            "root",
            details,
            Language::English,
            false,
        )
        .unwrap();
        assert!(review.summary().contains("Agent host tool"));
        assert!(review.allow_once);
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
        let mut app = App::new(true, Theme::Mono, Language::English, None);
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
        let mut app = App::new(true, Theme::Mono, Language::English, None);
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
        let mut app = App::new(true, Theme::Mono, Language::English, None);
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
