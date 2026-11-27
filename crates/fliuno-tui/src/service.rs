use crate::options::Options;
use crossbeam_channel::{Receiver, RecvTimeoutError, Sender, bounded};
use fliuno_client::{Client, Error as ClientError, EventFeed};
use fliuno_protocol::{
    ApprovalDecision, ApprovalDetails, ApprovalSummary, ArtifactDigest, ArtifactGetParams,
    ArtifactRevision, ArtifactSummary, ObjectPage, Session, SessionHistoryRole, Task, TaskPlan,
    TaskResumeDisposition, TaskResumeStatus, TaskStatus, TaskSteeringState, TaskSummary,
    TaskSummaryBatchParams, WorkSnapshot, WorkSnapshotParams, Workspace,
    WorkspaceHistorySearchParams,
};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use std::{
    fs,
    io::BufReader,
    path::PathBuf,
    process::{Child, ChildStdin, ChildStdout, Command, Stdio},
    sync::{Arc, Mutex},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

const REFRESH: Duration = Duration::from_millis(750);
const MAX_PREVIEW: u64 = 64 * 1024;
const MAX_DOWNLOAD: u64 = 32 * 1024 * 1024;
type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;
type ProcessSlot = Arc<Mutex<Option<Child>>>;

pub enum Request {
    Reconnect,
    Workspaces(u64),
    OpenWorkspace(String),
    Sessions {
        workspace: String,
        after: u64,
        browse: bool,
    },
    NewSession(String, String),
    SelectSession(String),
    Task(String),
    Start {
        session: String,
        text: String,
        key: String,
        input: Option<ArtifactGetParams>,
    },
    Approval(String, String),
    Decide(String, String, ApprovalDecision),
    Answer(String, String, String),
    Steer(String, String),
    Cancel(String),
    Resume(String),
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
}

pub struct Preview {
    pub id: ArtifactGetParams,
    pub digest: ArtifactDigest,
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

pub enum Update {
    Online(Workspace),
    Workspaces(ObjectPage<Workspace>),
    Workspace(Workspace),
    Sessions {
        workspace: String,
        page: ObjectPage<Session>,
        browse: bool,
    },
    Session(Session),
    Overview(String, WorkSnapshot),
    Detail(Box<Detail>),
    Inbox(WorkspaceInbox),
    Approvals(Vec<ApprovalSummary>, bool),
    Jump(Session, String),
    History(HistoryPage, bool),
    Approval(String, String, Box<ApprovalDetails>),
    Preview(Box<Preview>),
    Started(String),
    Notice(String),
    Finished,
    Error(String),
    Offline(String),
}

pub struct Worker {
    commands: Option<Sender<Request>>,
    pub updates: Receiver<Update>,
    child: ProcessSlot,
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
        let join = thread::Builder::new()
            .name("fliuno-connection".into())
            .spawn(move || serve(options, requests, out, slot))?;
        Ok(Self {
            commands: Some(commands),
            updates,
            child,
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
    client: Client<BufReader<ChildStdout>, ChildStdin>,
    workspace: Option<String>,
    session: Option<String>,
    inbox_visible: bool,
    approvals: Option<fliuno_client::ApprovalInbox>,
    approval_ids: Vec<String>,
    feed: Option<EventFeed>,
    watermark: Option<u64>,
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
    if options.demo {
        command.env("FLIUNO_PROVIDER", "demo");
    }
    let mut child = command.spawn()?;
    let input = child.stdin.take().expect("piped stdin");
    let output = child.stdout.take().expect("piped stdout");
    *slot
        .lock()
        .map_err(|_| "app-server process lock unavailable")? = Some(child);
    let client = Client::connect(BufReader::new(output), input, "Fliuno Workbench", |_| {})?;
    Ok(Service {
        client,
        workspace: None,
        session: None,
        inbox_visible: false,
        approvals: None,
        approval_ids: Vec::new(),
        feed: None,
        watermark: None,
    })
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

fn serve(options: Options, requests: Receiver<Request>, out: Sender<Update>, slot: ProcessSlot) {
    let mut root = options.workspace.to_string_lossy().into_owned();
    let mut service = None;
    let mut first = true;
    loop {
        let incoming = if first {
            first = false;
            Ok(Request::Reconnect)
        } else {
            requests.recv_timeout(REFRESH)
        };
        let mut finished = true;
        let result = match incoming {
            Err(RecvTimeoutError::Disconnected) => break,
            Err(RecvTimeoutError::Timeout) => {
                finished = false;
                service
                    .as_mut()
                    .map_or(Ok(()), |server: &mut Service| server.refresh(&out, false))
            }
            Ok(Request::Reconnect) => {
                close(&mut service, &slot);
                match connect(&options, &slot).and_then(|mut server| {
                    let workspace: Workspace =
                        server.rpc("workspace/open", json!({"root":root}))?;
                    server.workspace = Some(workspace.id.clone());
                    service = Some(server);
                    out.send(Update::Online(workspace))?;
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
            Ok(request) => service
                .as_mut()
                .ok_or_else(|| "Connect to the app-server first".into())
                .and_then(|server| server.handle(request, &out, &mut root)),
        };
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
        if finished && out.send(Update::Finished).is_err() {
            break;
        }
    }
    close(&mut service, &slot);
}

impl Service {
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
        Ok(self.client.call(method, &params, |notice| {
            if let Some(feed) = feed {
                feed.observe(notice);
            }
        })?)
    }

    fn refresh(&mut self, out: &Sender<Update>, force: bool) -> Result<()> {
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
        let (snapshot, approvals) = if self.watermark.is_none() {
            let sub = self.client.subscribe_work(&params, |_| {})?;
            self.feed = Some(sub.events);
            (sub.snapshot, sub.inbox)
        } else {
            let feed = &mut self.feed;
            let (snapshot, approvals) = self.client.work_snapshot(&params, |notice| {
                if let Some(feed) = feed {
                    feed.observe(notice);
                }
            })?;
            if let Some(feed) = self.feed.as_mut() {
                // Updated summaries come from the snapshot; this cursor closes gaps
                // without retaining event bodies or replaying client actions.
                let _ = feed.next_task_changes(&mut self.client, 50, |_| {})?;
            }
            (snapshot, approvals)
        };
        let watermark = snapshot.approvals.snapshot.through_sequence;
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
        Ok(())
    }

    fn handle(&mut self, request: Request, out: &Sender<Update>, root: &mut String) -> Result<()> {
        let update = match request {
            Request::Reconnect => unreachable!(),
            Request::Workspaces(after) => {
                Update::Workspaces(self.rpc("workspace/list", json!({"after":after,"limit":30}))?)
            }
            Request::OpenWorkspace(path) => {
                let workspace: Workspace = self.rpc("workspace/open", json!({"root":path}))?;
                *root = workspace.root.clone();
                self.workspace = Some(workspace.id.clone());
                self.session = None;
                self.inbox_visible = false;
                self.approvals = None;
                self.feed = None;
                self.watermark = None;
                Update::Workspace(workspace)
            }
            Request::Sessions {
                workspace,
                after,
                browse,
            } => {
                let page = self.rpc("session/list", json!({"workspaceId":workspace,"after":after,"order":"newest_first","archived":false,"limit":30}))?;
                Update::Sessions {
                    workspace,
                    page,
                    browse,
                }
            }
            Request::NewSession(workspace, title) => Update::Session(self.rpc(
                "session/create",
                json!({"workspaceId":workspace,"title":title,"clientRequestId":request_id()}),
            )?),
            Request::SelectSession(session) => {
                self.session = Some(session);
                self.inbox_visible = false;
                self.approvals = None;
                self.approval_ids.clear();
                self.feed = None;
                self.watermark = None;
                return self.refresh(out, true);
            }
            Request::Task(id) => Update::Detail(Box::new(self.detail(&id)?)),
            Request::Start {
                session,
                text,
                key,
                input,
            } => {
                let context = self
                    .supports("taskInteraction")
                    .then(|| json!({"enableInteraction":true}));
                let task: Task = self.rpc("task/start", json!({"sessionId":session,"input":text,"clientRequestId":key,"context":context,"artifactInputs":input.into_iter().collect::<Vec<_>>()}))?;
                out.send(Update::Started(task.id.clone()))?;
                out.send(Update::Detail(Box::new(self.detail(&task.id)?)))?;
                return self.refresh(out, true);
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
            Request::Resume(task) => {
                let status: TaskResumeStatus =
                    self.rpc("task/getResumeStatus", json!({"taskId":task}))?;
                if status.disposition != TaskResumeDisposition::Ready {
                    return Err(format!("Cannot safely resume: {:?}", status.disposition).into());
                }
                let _: Value = self.rpc("task/resume", json!({"taskId":task}))?;
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
                staging.persist_noclobber(&path)?;
                Update::Notice(format!("Saved {}", path.display()))
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
        Ok(Detail {
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
            revision,
            text,
        })
    }
}
