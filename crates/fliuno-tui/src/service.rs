use crate::activity;
use crate::backup::{self, BackupRequest, BackupResult};
use crate::{
    health,
    options::Options,
    pipe::{PipeInbox, PipeReader},
    task_browser::PageAction,
};
use crossbeam_channel::{Receiver, Sender, TryRecvError, bounded, select_biased};
use fliuno_client::{Client, Error as ClientError, EventFeed, SessionHistory};
use fliuno_protocol::{
    ApprovalDecision, ApprovalDetails, ApprovalSummary, ArtifactCatalogScope,
    ArtifactCatalogSnapshotPage, ArtifactCatalogSnapshotParams, ArtifactDigest, ArtifactGetParams,
    ArtifactRevision, ArtifactSummary, ContextOptions, Event, ModelCatalogPreview,
    ModelConnectionCheck, ModelConnectionCheckParams, ModelProfileConnectionGetParams,
    ModelProfileConnectionPreviewParams, ModelProfileConnectionSaveParams,
    ModelProfileConnectionTarget, ModelProfileConnectionView, ModelProfileCredentialInput,
    ModelProfileDefault, ModelProfileEditGetParams, ModelProfileEditParams, ModelProfileEditView,
    ModelProfileEntry, ModelProfileList, ModelProfileSelectParams, ModelUsageSummary,
    ModelUsageTotals, ObjectPage, ProviderModelListParams, ProviderModelPage, Session,
    SessionContextParams, SessionForkParams, SessionHistoryListPage, SessionHistoryListParams,
    SessionHistoryRole, SessionListOrder, SessionListParams, Task, TaskContext,
    TaskContextAttribution, TaskPlan, TaskQueue, TaskQueueItem, TaskQueueItemParams,
    TaskQueueState, TaskReconcileParams, TaskReconciliation, TaskResumeDisposition,
    TaskResumeParams, TaskResumeStatus, TaskStatus, TaskSteeringState, TaskSummary,
    TaskSummaryBatchParams, TaskTimelinePage, TaskTimelineParams, WorkSnapshot, WorkSnapshotParams,
    Workspace, WorkspaceHistorySearchParams,
};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use std::{
    fs,
    path::PathBuf,
    process::{Child, ChildStdin, Command, Stdio},
    rc::Rc,
    sync::{Arc, Mutex},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

const POLL_REFRESH: Duration = Duration::from_millis(750);
const SUBSCRIPTION_REFRESH: Duration = Duration::from_secs(20);
const MAX_PREVIEW: u64 = 64 * 1024;
const MAX_DOWNLOAD: u64 = 32 * 1024 * 1024;
pub const TRANSCRIPT_PAGE_TURNS: u32 = 12;
pub const TRANSCRIPT_MAX_TURNS: usize = 96;
type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;
type ProcessSlot = Arc<Mutex<Option<Child>>>;

pub enum Request {
    Reconnect,
    Diagnostics,
    ModelProfiles,
    OpenModelConnection(ModelProfileConnectionGetParams),
    PreviewModelConnection(ModelProfileConnectionPreviewParams),
    SaveModelConnection(ModelProfileConnectionSaveParams),
    EditModel(ModelProfileEditGetParams),
    SaveModelEdit(ModelProfileEditParams),
    BrowseModels(ProviderModelListParams),
    CheckModel(ModelConnectionCheckParams),
    SaveModelDefault(ModelProfileDefault),
    SelectModel(ModelProfileSelectParams),
    Memories {
        workspace: String,
        query: String,
        after: u64,
    },
    SaveMemory(fliuno_protocol::WorkspaceMemoryPutParams),
    Deliverables(ArtifactCatalogSnapshotParams),
    Backup(BackupRequest),
    Workspaces(u64),
    OpenWorkspace(String),
    Sessions(SessionListParams, bool),
    ForkSession(SessionForkParams),
    UpdateSession {
        session: Session,
        title: Option<String>,
        catalog: SessionListParams,
    },
    NewSession(String, Option<String>),
    SelectSession(String),
    OlderTranscript(SessionHistoryListParams),
    Task(String),
    TaskBrowser(fliuno_protocol::TaskSummaryListParams, PageAction),
    Start {
        session: String,
        text: String,
        key: String,
        input: Option<ArtifactGetParams>,
        plan_only: bool,
        context: Box<ContextOptions>,
        enqueue: bool,
    },
    Queue(String),
    Timeline(TaskTimelineParams),
    Event(String, u64),
    ContextAttribution(String),
    DraftContext(Box<SessionContextParams>),
    StartQueued(TaskQueueItemParams),
    CancelQueued(TaskQueueItemParams),
    Approval(String, String),
    Decide(String, String, ApprovalDecision),
    Answer(String, String, String),
    Steer(String, String),
    Cancel(String),
    Resume(TaskResumeParams),
    Reconcile(TaskReconcileParams),
    MoreApprovals,
    WorkspaceInbox(bool),
    OpenTask {
        session: String,
        task: String,
        approval: Option<String>,
    },
    SearchHistory {
        query: String,
        role: Option<SessionHistoryRole>,
        through_sequence: Option<u64>,
        before_sequence: Option<u64>,
        append: bool,
    },
    Preview(ArtifactGetParams),
    Download(ArtifactGetParams, PathBuf),
}

pub struct Detail {
    pub task: Task,
    pub plan: Option<TaskPlan>,
    pub artifacts: Vec<ArtifactSummary>,
    pub resume: Option<TaskResumeStatus>,
    pub review: Option<TaskReconciliation>,
    pub can_reconcile: bool,
    pub usage: Option<ModelUsageTotals>,
}

pub struct Preview {
    pub id: ArtifactGetParams,
    pub digest: ArtifactDigest,
    pub summary: ArtifactSummary,
    pub revision: Option<ArtifactRevision>,
    pub text: Option<String>,
}

#[derive(Clone)]
pub struct WorkspaceInbox {
    pub snapshot: WorkSnapshot,
}

#[derive(Clone)]
pub struct HistoryItem {
    pub task: TaskSummary,
    pub completion_sequence: u64,
}

#[derive(Clone)]
pub struct HistoryPage {
    pub query: String,
    pub role: Option<SessionHistoryRole>,
    pub through_sequence: u64,
    pub next_before_sequence: Option<u64>,
    pub scanned_turns: u32,
    pub skipped_oversized_turns: u32,
    pub items: Vec<HistoryItem>,
}

pub enum TranscriptPage {
    Recent(SessionHistoryListPage),
    Newer(SessionHistoryListPage),
    CaughtUp(SessionHistoryListPage),
    Older(SessionHistoryListPage),
}

pub struct ModelProfileFeatures {
    pub edit: bool,
    pub connection: bool,
}

pub enum Update {
    Online {
        workspace: Workspace,
        model: Option<String>,
        model_profiles: bool,
    },
    ModelProfiles(
        ModelProfileList,
        Option<ModelProfileDefault>,
        ModelProfileFeatures,
    ),
    ModelConnectionOpened(ModelProfileConnectionTarget, ModelProfileConnectionView),
    ModelConnectionPreviewed(ModelCatalogPreview),
    ModelConnectionSaved(ModelProfileEntry),
    ModelEditOpened(ModelProfileEditView),
    ModelEditSaved(ModelProfileEditView),
    ModelCatalogPage(ProviderModelPage),
    ModelDefaultSaved(ModelProfileDefault),
    ModelChecked {
        profile: Option<String>,
        check: ModelConnectionCheck,
    },
    ModelSelected(ModelProfileEntry),
    ModelProfileError(String),
    Workspaces(ObjectPage<Workspace>),
    Workspace(Workspace),
    Sessions {
        workspace: String,
        page: ObjectPage<Session>,
        browse: bool,
    },
    Session(Session),
    SessionMetadata(Session),
    Overview(String, WorkSnapshot),
    Transcript(String, TranscriptPage),
    Detail(Box<Detail>),
    TaskBrowser {
        session: String,
        after: u64,
        action: PageAction,
        page: ObjectPage<TaskSummary>,
    },
    Inbox(WorkspaceInbox),
    Approvals(Vec<ApprovalSummary>, bool),
    Jump(Session, String),
    History(HistoryPage, bool),
    Approval(String, String, Box<ApprovalDetails>),
    Preview(Box<Preview>),
    Started(String),
    Enqueued(TaskQueueItem),
    Queue(TaskQueue),
    Timeline(TaskTimelinePage),
    Event(u64, String),
    ContextAttribution(Box<TaskContextAttribution>),
    DraftContext(String, Box<TaskContext>),
    StartedQueued(String),
    Notice(String),
    Reconciled(String),
    BackupDone(Box<BackupResult>),
    Diagnostics(String),
    Activity(Vec<activity::Preview>),
    Memories {
        workspace: String,
        query: String,
        page: ObjectPage<fliuno_protocol::WorkspaceMemory>,
    },
    MemorySaved(String),
    Deliverables {
        workspace: String,
        page: ArtifactCatalogSnapshotPage,
    },
    Saved {
        path: PathBuf,
        sha256: String,
    },
    Finished,
    Error(String),
    Offline(String),
}

pub struct Worker {
    commands: Option<Sender<Request>>,
    pub updates: Receiver<Update>,
    child: ProcessSlot,
    backup_child: backup::ProcessSlot,
    join: Option<JoinHandle<()>>,
}

fn request_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

impl Worker {
    pub fn spawn(options: Options) -> std::io::Result<Self> {
        let (commands, requests) = bounded(4);
        let (out, updates) = bounded(16);
        let child = Arc::new(Mutex::new(None));
        let slot = Arc::clone(&child);
        let backup_child = Arc::new(Mutex::new(backup::BackupProcess::default()));
        let backup_slot = Arc::clone(&backup_child);
        let join = thread::Builder::new()
            .name("fliuno-connection".into())
            .spawn(move || serve(options, requests, out, slot, backup_slot))?;
        Ok(Self {
            commands: Some(commands),
            updates,
            child,
            backup_child,
            join: Some(join),
        })
    }

    pub fn send(&self, request: Request) -> bool {
        self.commands
            .as_ref()
            .is_some_and(|sender| sender.try_send(request).is_ok())
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.commands.take();
        if let Ok(mut backup_child) = self.backup_child.lock() {
            backup_child.close();
        }
        let Some(join) = self.join.take() else { return };
        let deadline = Instant::now() + Duration::from_secs(2);
        while !join.is_finished() && Instant::now() < deadline {
            let _ = self.updates.recv_timeout(Duration::from_millis(20));
        }
        if !join.is_finished() {
            if let Some(child) = self.child.lock().ok().and_then(|mut slot| slot.take()) {
                finish_child(child, Duration::ZERO);
            }
            // A disconnected receiver also wakes any pending worker send.
            self.updates = bounded(0).1;
        }
        let _ = join.join();
    }
}

struct Service {
    client: Client<PipeReader, ChildStdin>,
    inbox: Rc<PipeInbox>,
    next_refresh: Instant,
    workspace: Option<String>,
    session: Option<String>,
    inbox_visible: bool,
    approvals: Option<fliuno_client::ApprovalInbox>,
    approval_ids: Vec<String>,
    feed: Option<EventFeed>,
    watermark: Option<u64>,
    transcript: Option<TranscriptHead>,
    activity: activity::Progress,
    selected_model: Option<ModelProfileEntry>,
}

struct TranscriptHead {
    through: u64,
    completed: u64,
}

fn finish_child(mut child: Child, grace: Duration) {
    let deadline = Instant::now() + grace;
    loop {
        if matches!(child.try_wait(), Ok(Some(_))) {
            return;
        }
        if Instant::now() >= deadline {
            break;
        }
        thread::sleep(Duration::from_millis(20));
    }
    let _ = child.kill();
    let _ = child.wait();
}

fn close(service: &mut Option<Service>, slot: &ProcessSlot) {
    drop(service.take());
    let child = slot.lock().ok().and_then(|mut slot| slot.take());
    if let Some(child) = child {
        finish_child(child, Duration::from_millis(900));
    }
}

fn connect(options: &Options, slot: &ProcessSlot) -> Result<Service> {
    if let Some(parent) = options
        .database
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
    {
        fs::create_dir_all(parent)?;
    }
    let mut command = Command::new(fs::canonicalize(&options.server).map_err(|error| {
        format!(
            "Cannot open app-server at {}: {error}",
            options.server.display()
        )
    })?);
    command
        .arg("--db")
        .arg(&options.database)
        .args(&options.server_args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    if options.force_demo {
        command.arg("--demo");
    } else if let Some(config) = &options.config {
        command.arg("--config").arg(config);
        if let Some(profile) = &options.model_profile {
            command.arg("--model-profile").arg(profile);
        }
    }
    let mut child = command.spawn()?;
    let input = child.stdin.take().expect("piped stdin");
    let output = child.stdout.take().expect("piped stdout");
    *slot
        .lock()
        .map_err(|_| "app-server process lock unavailable")? = Some(child);
    let (reader, inbox) = PipeReader::open(output)?;
    let client = Client::connect(reader, input, "Fliuno Workbench", |_| {})?;
    Ok(Service {
        client,
        inbox,
        next_refresh: Instant::now() + POLL_REFRESH,
        workspace: None,
        session: None,
        inbox_visible: false,
        approvals: None,
        approval_ids: Vec::new(),
        feed: None,
        watermark: None,
        transcript: None,
        activity: activity::Progress::default(),
        selected_model: None,
    })
}

fn clear_model_key(input: &mut ModelProfileCredentialInput) {
    if let ModelProfileCredentialInput::Managed { key } = input {
        zeroize::Zeroize::zeroize(key);
    }
}

fn model_profile_update<T>(result: Result<T>, success: impl FnOnce(T) -> Update) -> Result<Update> {
    match result {
        Ok(value) => Ok(success(value)),
        Err(error) if !disconnected(error.as_ref()) => {
            Ok(Update::ModelProfileError(profile_error(error.as_ref())))
        }
        Err(error) => Err(error),
    }
}

fn profile_error(error: &(dyn std::error::Error + 'static)) -> String {
    if let Some(ClientError::Rpc { message, .. }) = error.downcast_ref::<ClientError>() {
        message
            .strip_prefix("state conflict: ")
            .or_else(|| message.strip_prefix("invalid argument: "))
            .unwrap_or(message)
            .to_owned()
    } else {
        error.to_string()
    }
}

fn disconnected(err: &(dyn std::error::Error + 'static)) -> bool {
    matches!(
        err.downcast_ref::<ClientError>(),
        Some(
            ClientError::Closed
                | ClientError::Io(_)
                | ClientError::InvalidFrame(_)
                | ClientError::ResponseTooLarge
        )
    )
}

#[allow(
    clippy::large_enum_variant,
    reason = "Keep the single pending command on the stack"
)]
enum Incoming {
    Command(Request),
    Notification,
    Refresh,
}

fn serve(
    options: Options,
    requests: Receiver<Request>,
    out: Sender<Update>,
    slot: ProcessSlot,
    backup_slot: backup::ProcessSlot,
) {
    let mut root = options.workspace.to_string_lossy().into_owned();
    let mut service: Option<Service> = None;
    let mut incoming = Some(Incoming::Command(Request::Reconnect));
    while let Some(input) = incoming {
        let finished = matches!(input, Incoming::Command(_));
        let result = match input {
            Incoming::Refresh => Ok(()),
            Incoming::Notification => service.as_mut().map_or(Ok(()), Service::read_notices),
            Incoming::Command(Request::Reconnect) => {
                close(&mut service, &slot);
                match connect(&options, &slot).and_then(|mut server| {
                    let workspace: Workspace =
                        server.rpc("workspace/open", json!({"root":root}))?;
                    server.workspace = Some(workspace.id.clone());
                    let model = server.client.info().capabilities["modelCapabilities"]["model"]
                        .as_str()
                        .map(str::to_owned);
                    let model_profiles = server.supports("modelProfiles");
                    service = Some(server);
                    out.send(Update::Online {
                        workspace,
                        model,
                        model_profiles,
                    })?;
                    Ok(())
                }) {
                    Ok(()) => Ok(()),
                    Err(error) => {
                        close(&mut service, &slot);
                        out.send(Update::Offline(format!("Cannot connect: {error}")))
                            .map_err(Into::into)
                    }
                }
            }
            Incoming::Command(Request::Diagnostics) => {
                let info = service.as_ref().map(|server| server.client.info());
                let selected_model = service
                    .as_ref()
                    .and_then(|server| server.selected_model.as_ref());
                let report = health::inspect(&options, info, selected_model);
                out.send(Update::Diagnostics(report.text))
                    .map_err(Into::into)
            }
            Incoming::Command(Request::Backup(request)) => {
                backup::run(&options, request, &backup_slot)
                    .and_then(|result| Ok(out.send(Update::BackupDone(Box::new(result)))?))
            }
            Incoming::Command(request) => service
                .as_mut()
                .ok_or_else(|| "Connect to the app-server first".into())
                .and_then(|server| server.handle(request, &out, &mut root)),
        };
        let result = result.and_then(|()| {
            if let Some(server) = &mut service
                && server.needs_refresh()
            {
                server.refresh(&out, false)?;
            }
            Ok(())
        });
        if let Err(error) = result {
            let update = if disconnected(error.as_ref()) {
                close(&mut service, &slot);
                Update::Offline(format!("Connection lost: {error}"))
            } else {
                Update::Error(error.to_string())
            };
            if out.send(update).is_err() {
                break;
            }
        }
        if let Some(previews) = service
            .as_mut()
            .and_then(|server| server.activity.changed())
            && out.send(Update::Activity(previews)).is_err()
        {
            break;
        }
        if finished && out.send(Update::Finished).is_err() {
            break;
        }
        incoming = match &service {
            Some(server) => server.wait(&requests),
            None => requests.recv().ok().map(Incoming::Command),
        };
    }
    close(&mut service, &slot);
}

fn observe(
    feed: &mut Option<EventFeed>,
    activity: &mut activity::Progress,
    notice: fliuno_client::Notification,
) {
    let notice = match feed {
        Some(feed) => feed.observe(notice),
        None => Some(notice),
    };
    if let Some(notice) = notice {
        activity.observe(notice);
    }
}

impl Service {
    fn wait(&self, requests: &Receiver<Request>) -> Option<Incoming> {
        match requests.try_recv() {
            Ok(request) => return Some(Incoming::Command(request)),
            Err(TryRecvError::Disconnected) => return None,
            Err(TryRecvError::Empty) => {}
        }
        if self.inbox.buffered.get() {
            return Some(Incoming::Notification);
        }
        let timeout = self.next_refresh.saturating_duration_since(Instant::now());
        select_biased! {
            recv(requests) -> request => request.ok().map(Incoming::Command),
            recv(self.inbox.chunks) -> chunk => {
                self.inbox.staged.set(chunk.ok());
                Some(Incoming::Notification)
            }
            default(timeout) => Some(Incoming::Refresh),
        }
    }

    fn read_notices(&mut self) -> Result<()> {
        // Let bursts share a single snapshot while keeping commands responsive.
        for _ in 0..32 {
            let notice = self.client.read_notification()?;
            observe(&mut self.feed, &mut self.activity, notice);
            if !self.inbox.buffered.get() && self.inbox.chunks.is_empty() {
                break;
            }
        }
        Ok(())
    }

    fn needs_refresh(&self) -> bool {
        Instant::now() >= self.next_refresh
            || self.activity.gap
            || self
                .feed
                .as_ref()
                .is_some_and(|feed| feed.snapshot_stale(self.watermark.unwrap_or_default()))
    }

    fn supports(&self, name: &str) -> bool {
        self.client
            .info()
            .capabilities
            .get(name)
            .and_then(Value::as_bool)
            == Some(true)
    }

    fn rpc<T: DeserializeOwned>(&mut self, method: &str, params: Value) -> Result<T> {
        let feed = &mut self.feed;
        let activity = &mut self.activity;
        Ok(self
            .client
            .call(method, &params, |notice| observe(feed, activity, notice))?)
    }

    fn model_connection_rpc<T: DeserializeOwned>(
        &mut self,
        method: &str,
        params: &impl serde::Serialize,
    ) -> Result<T> {
        Ok(self.client.call(method, params, |notice| {
            observe(&mut self.feed, &mut self.activity, notice);
        })?)
    }

    fn refresh(&mut self, out: &Sender<Update>, force: bool) -> Result<()> {
        let interval = if self.supports("eventWatermarkNotifications") {
            SUBSCRIPTION_REFRESH
        } else {
            POLL_REFRESH
        };
        self.next_refresh = Instant::now() + interval;
        if std::mem::take(&mut self.activity.gap) && self.supports("modelProgressResubscribe") {
            let _: Value = self.rpc("modelProgress/resubscribe", json!({}))?;
        }
        let (workspace_id, session_id) = if self.inbox_visible {
            let workspace = self.workspace.clone().ok_or("Open a workspace first")?;
            (Some(workspace), None)
        } else if let Some(session) = self.session.clone() {
            (None, Some(session))
        } else {
            return Ok(());
        };
        let params = WorkSnapshotParams {
            workspace_id,
            session_id: session_id.clone(),
            approval_limit: if self.inbox_visible { 100 } else { 25 },
            approval_target_bytes: Some(64 * 1024),
            recent_task_limit: Some(50),
            include_actions: self.supports("workNextActions"),
        };
        let resubscribe = self.watermark.is_none()
            || self.feed.as_ref().is_some_and(EventFeed::subscription_lost);
        let (snapshot, approvals) = if resubscribe {
            let activity = &mut self.activity;
            let sub = self
                .client
                .subscribe_work(&params, |notice| activity.observe(notice))?;
            self.feed = Some(sub.events);
            (sub.snapshot, sub.inbox)
        } else {
            let feed = &mut self.feed;
            let activity = &mut self.activity;
            // The snapshot supersedes intermediate events, including approval state.
            self.client
                .work_snapshot(&params, |notice| observe(feed, activity, notice))?
        };
        let watermark = snapshot.approvals.snapshot.through_sequence;
        let completed = snapshot.task_counts.completed;
        let approval_ids: Vec<_> = snapshot
            .approvals
            .page
            .items
            .iter()
            .map(|approval| approval.id.clone())
            .collect();
        let approvals_changed = self.inbox_visible && self.approval_ids != approval_ids;
        if force || self.watermark != Some(watermark) || approvals_changed {
            self.approvals = Some(approvals);
            self.approval_ids = approval_ids;
            if self.inbox_visible {
                out.send(Update::Inbox(WorkspaceInbox { snapshot }))?;
            } else if let Some(session) = session_id {
                out.send(Update::Overview(session, snapshot))?;
            }
        }
        self.watermark = Some(watermark);
        if !self.inbox_visible
            && let Some(session) = &self.session
            && self.supports("liveModels")
            && self.supports("sessionHistoryList")
            && self
                .transcript
                .as_ref()
                .is_none_or(|head| head.completed != completed)
        {
            self.refresh_transcript(out, session.clone(), watermark, completed)?;
        }
        Ok(())
    }

    fn transcript_page(
        &mut self,
        params: SessionHistoryListParams,
    ) -> Result<SessionHistoryListPage> {
        self.next_transcript_page(&mut SessionHistory::new(params))?
            .ok_or_else(|| "Conversation history page unavailable".into())
    }

    fn next_transcript_page(
        &mut self,
        history: &mut SessionHistory,
    ) -> Result<Option<SessionHistoryListPage>> {
        let feed = &mut self.feed;
        let activity = &mut self.activity;
        Ok(history.next_page(&mut self.client, |notification| {
            observe(feed, activity, notification)
        })?)
    }

    fn refresh_transcript(
        &mut self,
        out: &Sender<Update>,
        session: String,
        through: u64,
        completed: u64,
    ) -> Result<()> {
        let after = self
            .transcript
            .as_ref()
            .filter(|_| self.supports("sessionHistoryAfterSequence"))
            .map(|head| head.through);
        let mut history = SessionHistory::new(SessionHistoryListParams {
            session_id: session.clone(),
            through_sequence: Some(through),
            after_sequence: after,
            before_sequence: None,
            limit: Some(TRANSCRIPT_PAGE_TURNS),
            max_bytes: None,
        });
        let first = self
            .next_transcript_page(&mut history)?
            .ok_or("Conversation history page unavailable")?;
        let page = if after.is_none() {
            if self.transcript.is_none() {
                TranscriptPage::Recent(first)
            } else {
                TranscriptPage::CaughtUp(first)
            }
        } else {
            self.catch_up_transcript(first, &mut history)?
        };
        self.transcript = Some(TranscriptHead { through, completed });
        out.send(Update::Transcript(session, page))?;
        Ok(())
    }

    fn catch_up_transcript(
        &mut self,
        mut first: SessionHistoryListPage,
        history: &mut SessionHistory,
    ) -> Result<TranscriptPage> {
        if first.next_before_sequence.is_none() {
            return Ok(TranscriptPage::Newer(first));
        }
        // Keep the newest page intact in case the backlog exceeds this view's capacity.
        let mut backlog = Vec::new();
        while let Some(page) = self.next_transcript_page(history)? {
            backlog.extend(page.entries);
            let count = first.entries.len() + backlog.len();
            if count > TRANSCRIPT_MAX_TURNS
                || (count == TRANSCRIPT_MAX_TURNS && page.next_before_sequence.is_some())
            {
                return Ok(TranscriptPage::CaughtUp(first));
            }
        }
        first.entries.extend(backlog);
        first.next_before_sequence = None;
        Ok(TranscriptPage::Newer(first))
    }

    fn sessions(&mut self, mut params: SessionListParams, browse: bool) -> Result<Update> {
        if !self.supports("sessionListOrdering")
            && params.order == Some(SessionListOrder::NewestFirst)
        {
            params.order = None;
        }
        if params.query.is_some() && !self.supports("sessionSearch") {
            return Err("This app-server does not support session search".into());
        }
        let page = self.rpc("session/list", json!(params))?;
        Ok(Update::Sessions {
            workspace: params.workspace_id,
            page,
            browse,
        })
    }

    fn require_memory_workspace(&self, workspace: &str) -> Result<()> {
        if !self.supports("workspaceMemory") || self.workspace.as_deref() != Some(workspace) {
            return Err("Workspace memories are unavailable".into());
        }
        Ok(())
    }

    fn require_task_queue(&self) -> Result<()> {
        if self.supports("taskQueue") {
            Ok(())
        } else {
            Err("This app-server does not support queued prompts".into())
        }
    }

    fn task_queue(&mut self, session: &str) -> Result<TaskQueue> {
        self.require_task_queue()?;
        self.rpc("taskQueue/list", json!({"sessionId":session}))
    }

    fn handle(&mut self, request: Request, out: &Sender<Update>, root: &mut String) -> Result<()> {
        let update = match request {
            Request::Reconnect | Request::Diagnostics | Request::Backup(_) => unreachable!(),
            Request::ModelProfiles => match self.rpc("modelProfiles/list", json!({})) {
                Ok(profiles) => {
                    let default = if self.supports("modelProfileDefaults") {
                        match self.rpc("modelProfiles/getDefault", json!({})) {
                            Ok(default) => Some(default),
                            Err(error) if !disconnected(error.as_ref()) => None,
                            Err(error) => return Err(error),
                        }
                    } else {
                        None
                    };
                    Update::ModelProfiles(
                        profiles,
                        default,
                        ModelProfileFeatures {
                            edit: self.supports("modelProfileEditing"),
                            connection: self.supports("modelProfileConnections"),
                        },
                    )
                }
                Err(error) if !disconnected(error.as_ref()) => {
                    Update::ModelProfileError(profile_error(error.as_ref()))
                }
                Err(error) => return Err(error),
            },
            Request::OpenModelConnection(params) => {
                let result = self.rpc("modelProfiles/getConnection", json!(&params));
                model_profile_update(result, |view| {
                    Update::ModelConnectionOpened(params.target, view)
                })?
            }
            Request::PreviewModelConnection(mut params) => {
                let result = self.model_connection_rpc("modelProfiles/previewConnection", &params);
                clear_model_key(&mut params.connection.credential);
                model_profile_update(result, Update::ModelConnectionPreviewed)?
            }
            Request::SaveModelConnection(mut params) => {
                let result = self.model_connection_rpc::<ModelProfileConnectionView>(
                    "modelProfiles/saveConnection",
                    &params,
                );
                clear_model_key(&mut params.connection.credential);
                model_profile_update(result, |saved| {
                    let provider = if saved.protocol.as_deref() == Some("chat_completions") {
                        "openai-chat".into()
                    } else {
                        saved.provider.unwrap_or_default()
                    };
                    let profile = match params.target {
                        ModelProfileConnectionTarget::Base => None,
                        ModelProfileConnectionTarget::Existing { profile }
                        | ModelProfileConnectionTarget::New { profile } => Some(profile),
                    };
                    Update::ModelConnectionSaved(ModelProfileEntry {
                        profile,
                        provider,
                        model: saved.model.unwrap_or_default(),
                        reasoning_effort: saved.reasoning_effort,
                    })
                })?
            }
            Request::SaveModelDefault(default) => {
                match self.rpc("modelProfiles/setDefault", json!(default)) {
                    Ok(saved) => Update::ModelDefaultSaved(saved),
                    Err(error) if !disconnected(error.as_ref()) => {
                        Update::ModelProfileError(profile_error(error.as_ref()))
                    }
                    Err(error) => return Err(error),
                }
            }
            Request::EditModel(params) => match self.rpc("modelProfiles/getEdit", json!(params)) {
                Ok(result) => Update::ModelEditOpened(result),
                Err(error) if !disconnected(error.as_ref()) => {
                    Update::ModelProfileError(profile_error(error.as_ref()))
                }
                Err(error) => return Err(error),
            },
            Request::SaveModelEdit(params) => {
                match self.rpc("modelProfiles/saveEdit", json!(params)) {
                    Ok(result) => Update::ModelEditSaved(result),
                    Err(error) if !disconnected(error.as_ref()) => {
                        Update::ModelProfileError(profile_error(error.as_ref()))
                    }
                    Err(error) => return Err(error),
                }
            }
            Request::BrowseModels(params) => match self.rpc("models/list", json!(params)) {
                Ok(result) => Update::ModelCatalogPage(result),
                Err(error) if !disconnected(error.as_ref()) => {
                    Update::ModelProfileError(profile_error(error.as_ref()))
                }
                Err(error) => return Err(error),
            },
            Request::CheckModel(params) => match self.rpc("models/check", json!(params)) {
                Ok(check) => Update::ModelChecked {
                    profile: params.profile,
                    check,
                },
                Err(error) if !disconnected(error.as_ref()) => {
                    Update::ModelProfileError(profile_error(error.as_ref()))
                }
                Err(error) => return Err(error),
            },
            Request::SelectModel(params) => {
                match self.rpc::<ModelProfileEntry>("modelProfiles/select", json!(params)) {
                    Ok(model) => {
                        self.selected_model = Some(model.clone());
                        Update::ModelSelected(model)
                    }
                    Err(error) if !disconnected(error.as_ref()) => {
                        Update::ModelProfileError(profile_error(error.as_ref()))
                    }
                    Err(error) => return Err(error),
                }
            }
            Request::Workspaces(after) => {
                Update::Workspaces(self.rpc("workspace/list", json!({"after":after,"limit":30}))?)
            }
            Request::Memories {
                workspace,
                query,
                after,
            } => {
                self.require_memory_workspace(&workspace)?;
                let (method, params) = if query.is_empty() {
                    (
                        "workspace/memory/list",
                        json!({"workspaceId":workspace,"after":after,"limit":20}),
                    )
                } else {
                    (
                        "workspace/memory/search",
                        json!({"workspaceId":workspace,"query":query,"after":after,"limit":20}),
                    )
                };
                let page: ObjectPage<fliuno_protocol::WorkspaceMemory> =
                    self.rpc(method, params)?;
                if page
                    .items
                    .iter()
                    .any(|record| record.workspace_id != workspace)
                {
                    return Err("Memory response outside the current workspace".into());
                }
                Update::Memories {
                    workspace,
                    query,
                    page,
                }
            }
            Request::SaveMemory(record) => {
                self.require_memory_workspace(&record.workspace_id)?;
                let _: fliuno_protocol::WorkspaceMemory =
                    self.rpc("workspace/memory/put", json!(record))?;
                Update::MemorySaved(record.workspace_id)
            }
            Request::OpenWorkspace(path) => {
                let workspace: Workspace = self.rpc("workspace/open", json!({"root":path}))?;
                *root = workspace.root.clone();
                self.workspace = Some(workspace.id.clone());
                self.session = None;
                self.transcript = None;
                self.inbox_visible = false;
                self.approvals = None;
                self.feed = None;
                self.watermark = None;
                Update::Workspace(workspace)
            }
            Request::Sessions(params, browse) => self.sessions(params, browse)?,
            Request::ForkSession(params) => {
                if !self.supports("sessionFork") {
                    return Err("This app-server does not support session forks".into());
                }
                Update::Session(self.rpc("session/fork", json!(params))?)
            }
            Request::UpdateSession {
                session,
                title,
                catalog,
            } => {
                let saved: Session = if let Some(title) = title {
                    self.rpc(
                        "session/rename",
                        json!({"sessionId":session.id,"title":title}),
                    )?
                } else {
                    self.rpc(
                        "session/setArchived",
                        json!({"sessionId":session.id,"archived":!session.archived}),
                    )?
                };
                out.send(Update::SessionMetadata(saved))?;
                self.sessions(catalog, true)?
            }
            Request::NewSession(workspace, title) => Update::Session(self.rpc(
                "session/create",
                json!({"workspaceId":workspace,"title":title,"clientRequestId":request_id()}),
            )?),
            Request::SelectSession(session) => {
                self.session = Some(session);
                self.transcript = None;
                self.inbox_visible = false;
                self.approvals = None;
                self.approval_ids.clear();
                self.feed = None;
                self.watermark = None;
                return self.refresh(out, true);
            }
            Request::OlderTranscript(params) => {
                if self.session.as_ref() != Some(&params.session_id) || self.inbox_visible {
                    return Err("Conversation history belongs to a different session".into());
                }
                let session = params.session_id.clone();
                Update::Transcript(
                    session,
                    TranscriptPage::Older(self.transcript_page(params)?),
                )
            }
            Request::Task(id) => Update::Detail(Box::new(self.detail(&id)?)),
            Request::TaskBrowser(mut params, action) => {
                if !self.supports("sessionTaskSummaryOrdering") {
                    params.order = None;
                }
                let page: ObjectPage<TaskSummary> =
                    self.rpc("task/listSummaries", json!(params))?;
                if page.items.iter().any(|item| {
                    item.session_id != params.session_id
                        || params.status.is_some_and(|status| status != item.status)
                }) {
                    return Err(
                        "The task list contained an item outside the requested filter".into(),
                    );
                }
                Update::TaskBrowser {
                    session: params.session_id,
                    after: params.after,
                    action,
                    page,
                }
            }
            Request::Start {
                session,
                text,
                key,
                input,
                plan_only,
                mut context,
                enqueue,
            } => {
                if plan_only && !self.supports("taskPlanOnly") {
                    return Err("This app-server does not support planning mode".into());
                }
                if *context != ContextOptions::default() && !self.supports("sessionContext") {
                    return Err("This app-server does not support draft context settings".into());
                }
                context.plan_only = plan_only;
                context.enable_interaction = !plan_only && self.supports("taskInteraction");
                let context = (*context != ContextOptions::default()).then_some(context);
                let params = json!({"sessionId":session,"input":text,"clientRequestId":key,
                    "context":context,"artifactInputs":input.into_iter().collect::<Vec<_>>()});
                if enqueue {
                    self.require_task_queue()?;
                    out.send(Update::Enqueued(self.rpc("taskQueue/enqueue", params)?))?;
                    return Ok(());
                }
                let task: Task = self.rpc("task/start", params)?;
                out.send(Update::Started(task.id.clone()))?;
                out.send(Update::Detail(Box::new(self.detail(&task.id)?)))?;
                return self.refresh(out, true);
            }
            Request::Timeline(params) => {
                if !self.supports("taskTimeline") {
                    return Err("This app-server does not support task timelines".into());
                }
                Update::Timeline(self.rpc("task/timeline", json!(params))?)
            }
            Request::Event(task, sequence) => {
                let event: Event = self.rpc("events/get", json!({"sequence":sequence}))?;
                if event.task_id != task || event.sequence != sequence {
                    return Err("The event does not belong to this task timeline".into());
                }
                Update::Event(sequence, serde_json::to_string_pretty(&event.data)?)
            }
            Request::DraftContext(params) => {
                if !self.supports("sessionContext") {
                    return Err("This app-server does not support context preview".into());
                }
                Update::DraftContext(
                    params.session_id.clone(),
                    Box::new(self.rpc("session/context", serde_json::to_value(params)?)?),
                )
            }
            Request::ContextAttribution(task) => {
                if !self.supports("taskContextAttribution") {
                    return Err("This app-server does not support context attribution".into());
                }
                Update::ContextAttribution(Box::new(
                    self.rpc("task/contextAttribution", json!({"taskId":task}))?,
                ))
            }
            Request::Queue(session) => Update::Queue(self.task_queue(&session)?),
            Request::StartQueued(target) => {
                self.require_task_queue()?;
                let receipt: TaskQueueItem = self.rpc("taskQueue/startNext", json!(target))?;
                if let TaskQueueState::Started { task_id } = receipt.status {
                    out.send(Update::StartedQueued(task_id.clone()))?;
                    out.send(Update::Detail(Box::new(self.detail(&task_id)?)))?;
                    return self.refresh(out, true);
                }
                out.send(Update::Notice("Prompt was cancelled".into()))?;
                Update::Queue(self.task_queue(&target.session_id)?)
            }
            Request::CancelQueued(target) => {
                self.require_task_queue()?;
                let _: TaskQueueItem = self.rpc("taskQueue/cancel", json!(target))?;
                out.send(Update::Notice("Queued prompt cancelled".into()))?;
                Update::Queue(self.task_queue(&target.session_id)?)
            }
            Request::Approval(task, approval) => {
                let details =
                    self.rpc("approval/get", json!({"taskId":task,"approvalId":approval}))?;
                Update::Approval(task, approval, Box::new(details))
            }
            Request::Decide(task, approval, decision) => {
                let _: Value = self.rpc(
                    "approval/respond",
                    json!({"taskId":task,"approvalId":approval,"decision":decision}),
                )?;
                return self.changed(&task, out);
            }
            Request::Answer(task, call, answer) => {
                let _: Value = self.rpc("task/respondToolInput", json!({"taskId":task,"callId":call,"requestId":request_id(),"responses":{"answer":answer}}))?;
                return self.changed(&task, out);
            }
            Request::Steer(task, instruction) => {
                let state: TaskSteeringState =
                    self.rpc("task/getSteering", json!({"taskId":task}))?;
                let _: Value = self.rpc("task/steer", json!({"taskId":task,"expectedSequence":state.through_sequence,"clientRequestId":request_id(),"instruction":instruction}))?;
                return self.changed(&task, out);
            }
            Request::Cancel(task) => {
                let _: Value = self.rpc("task/cancel", json!({"taskId":task}))?;
                return self.changed(&task, out);
            }
            Request::Resume(params) => {
                let task = params.task_id.clone();
                let status: TaskResumeStatus =
                    self.rpc("task/getResumeStatus", json!({"taskId":task}))?;
                let confirmed = if params.retry_uncertain_model_step {
                    TaskResumeDisposition::RetryUncertainModelStep
                } else {
                    TaskResumeDisposition::Ready
                };
                if status.disposition != confirmed {
                    return Err("The recovery state changed. Review the task again.".into());
                }
                let _: Task = self.rpc("task/resume", serde_json::to_value(params)?)?;
                return self.changed(&task, out);
            }
            Request::Reconcile(params) => {
                let task = params.task_id.clone();
                let _: TaskReconciliation =
                    self.rpc("task/reconcile", serde_json::to_value(params)?)?;
                out.send(Update::Reconciled(task.clone()))?;
                return self.changed(&task, out);
            }
            Request::MoreApprovals if !self.inbox_visible => {
                return Err("No workspace approval inbox is open".into());
            }
            Request::MoreApprovals => {
                let Some(inbox) = self.approvals.as_mut() else {
                    return Err("No workspace approval inbox is open".into());
                };
                let feed = &mut self.feed;
                let Some(page) = inbox.next_page(&mut self.client, |notice| {
                    if let Some(feed) = feed {
                        feed.observe(notice);
                    }
                })?
                else {
                    return Err("No more approval pages are available".into());
                };
                Update::Approvals(page.page.items, page.page.has_more)
            }
            Request::WorkspaceInbox(visible) => {
                self.inbox_visible = visible;
                self.approvals = None;
                self.approval_ids.clear();
                self.feed = None;
                self.watermark = None;
                return self.refresh(out, true);
            }
            Request::OpenTask {
                session,
                task,
                approval,
            } => {
                let Some(workspace) = self.workspace.clone() else {
                    return Err("Open a workspace first".into());
                };
                let target: Session = self.rpc("session/get", json!({"sessionId":session}))?;
                if target.workspace_id != workspace {
                    return Err("Task session is outside the active workspace".into());
                }
                let detail = self.detail(&task)?;
                if detail.task.session_id != target.id {
                    return Err("Task is outside the selected session".into());
                }
                self.session = Some(target.id.clone());
                self.transcript = None;
                self.inbox_visible = false;
                self.approvals = None;
                self.feed = None;
                self.watermark = None;
                out.send(Update::Jump(target, task.clone()))?;
                out.send(Update::Detail(Box::new(detail)))?;
                if let Some(approval) = approval {
                    let details: ApprovalDetails =
                        self.rpc("approval/get", json!({"taskId":task,"approvalId":approval}))?;
                    out.send(Update::Approval(task, approval, Box::new(details)))?;
                }
                return self.refresh(out, true);
            }
            Request::SearchHistory {
                query,
                role,
                through_sequence,
                before_sequence,
                append,
            } => {
                let Some(workspace_id) = self.workspace.clone() else {
                    return Err("Open a workspace first".into());
                };
                if !self.supports("workspaceHistorySearch") {
                    return Err(
                        "The connected app-server does not support workspace history search".into(),
                    );
                }
                let scan_limit = self
                    .client
                    .info()
                    .capabilities
                    .get("maxHistorySearchScanLimit")
                    .and_then(Value::as_u64)
                    .map(|limit| limit.min(128) as u32)
                    .filter(|limit| *limit > 0);
                let params = WorkspaceHistorySearchParams {
                    workspace_id: workspace_id.clone(),
                    query: query.clone(),
                    role,
                    through_sequence,
                    after_sequence: None,
                    before_sequence,
                    limit: Some(16),
                    scan_limit,
                };
                let mut search = fliuno_client::HistorySearch::new(
                    fliuno_client::HistorySearchRequest::Workspace(params),
                );
                let feed = &mut self.feed;
                let Some(result) = search.next_page(&mut self.client, |notice| {
                    if let Some(feed) = feed {
                        feed.observe(notice);
                    }
                })?
                else {
                    return Err("History search produced no page".into());
                };
                let ids: Vec<_> = result
                    .matches
                    .iter()
                    .map(|item| item.task_id.clone())
                    .collect();
                let summaries = if ids.is_empty() {
                    Vec::new()
                } else {
                    let params = TaskSummaryBatchParams {
                        task_ids: ids,
                        workspace_id: Some(workspace_id),
                        session_id: None,
                    };
                    let feed = &mut self.feed;
                    self.client
                        .get_task_summaries(&params, |notice| {
                            if let Some(feed) = feed {
                                feed.observe(notice);
                            }
                        })?
                        .items
                };
                let items = result
                    .matches
                    .into_iter()
                    .zip(summaries)
                    .filter_map(|(found, summary)| {
                        summary
                            .filter(|task| found.session_id.as_ref() == Some(&task.session_id))
                            .map(|task| HistoryItem {
                                task,
                                completion_sequence: found.completion_sequence,
                            })
                    })
                    .collect();
                Update::History(
                    HistoryPage {
                        query,
                        role,
                        through_sequence: result.through_sequence,
                        next_before_sequence: result.next_before_sequence,
                        scanned_turns: result.scanned_turns,
                        skipped_oversized_turns: result.skipped_oversized_turns,
                        items,
                    },
                    append,
                )
            }
            Request::Deliverables(params) => {
                let ArtifactCatalogScope::Workspace { workspace_id } = &params.scope else {
                    return Err("Open a workspace deliverable catalog".into());
                };
                if self.workspace.as_ref() != Some(workspace_id)
                    || !self.supports("artifactCatalogSnapshots")
                {
                    return Err("Workspace deliverables are unavailable".into());
                }
                let page = self.rpc("artifacts/listSnapshot", json!(params))?;
                Update::Deliverables {
                    workspace: workspace_id.clone(),
                    page,
                }
            }
            Request::Preview(id) => Update::Preview(Box::new(self.preview(id)?)),
            Request::Download(id, path) => {
                let digest = self.digest(&id)?;
                if digest.content_bytes > MAX_DOWNLOAD {
                    return Err("Artifact exceeds the 32 MiB download limit".into());
                }
                let parent = path
                    .parent()
                    .filter(|path| !path.as_os_str().is_empty())
                    .unwrap_or_else(|| std::path::Path::new("."));
                let mut staging = tempfile::NamedTempFile::new_in(parent)?;
                self.client
                    .download_artifact(&id, &digest, staging.as_file_mut(), |_| {})?;
                staging.as_file_mut().sync_all()?;
                staging.persist_noclobber(&path).map_err(
                    |failure| -> Box<dyn std::error::Error> {
                        if failure.error.kind() == std::io::ErrorKind::AlreadyExists {
                            return format!("Destination already exists: {}", path.display())
                                .into();
                        }
                        failure.into()
                    },
                )?;
                Update::Saved {
                    path,
                    sha256: digest.sha256,
                }
            }
        };
        out.send(update)?;
        Ok(())
    }

    fn changed(&mut self, id: &str, out: &Sender<Update>) -> Result<()> {
        out.send(Update::Detail(Box::new(self.detail(id)?)))?;
        self.refresh(out, true)
    }

    fn detail(&mut self, id: &str) -> Result<Detail> {
        let task: Task = self.rpc("task/get", json!({"taskId":id}))?;
        let plan = if self.supports("taskInteraction") {
            self.rpc("task/getPlan", json!({"taskId":id}))?
        } else {
            None
        };
        let page: ObjectPage<ArtifactSummary> =
            self.rpc("artifacts/listSummaries", json!({"taskId":id,"limit":100}))?;
        let resume = if task.status == TaskStatus::Interrupted {
            Some(self.rpc("task/getResumeStatus", json!({"taskId":id}))?)
        } else {
            None
        };
        let can_reconcile = self.supports("taskReconciliation")
            && task
                .agent_run
                .as_ref()
                .is_some_and(|run| run.finished && run.outcome_unknown);
        let review = if can_reconcile {
            self.rpc("task/getReconciliation", json!({"taskId":id}))?
        } else {
            None
        };
        let usage = if task.model_steps > 0 && self.supports("modelUsageSummary") {
            let report: ModelUsageSummary = self.rpc(
                "modelUsage/getSummary",
                json!({"scope":{"kind":"task","taskId":id}}),
            )?;
            (report.totals.report_count.0 != 0).then_some(report.totals)
        } else {
            None
        };
        Ok(Detail {
            usage,
            review,
            can_reconcile,
            task,
            plan,
            artifacts: page.items,
            resume,
        })
    }

    fn digest(&mut self, id: &ArtifactGetParams) -> Result<ArtifactDigest> {
        Ok(self.client.artifact_digest(id, |_| {})?)
    }

    fn preview(&mut self, id: ArtifactGetParams) -> Result<Preview> {
        let summary: ArtifactSummary =
            self.rpc("artifacts/getSummary", serde_json::to_value(&id)?)?;
        let digest = self.digest(&id)?;
        let revision = if self.supports("artifactRevisions") {
            self.rpc("artifacts/getRevision", serde_json::to_value(&id)?)?
        } else {
            None
        };
        let text = if digest.content_bytes <= MAX_PREVIEW && !summary.binary {
            let mut bytes = Vec::with_capacity(digest.content_bytes as usize);
            self.client
                .download_artifact(&id, &digest, &mut bytes, |_| {})?;
            Some(String::from_utf8(bytes)?)
        } else {
            None
        };
        Ok(Preview {
            id,
            digest,
            summary,
            revision,
            text,
        })
    }
}
