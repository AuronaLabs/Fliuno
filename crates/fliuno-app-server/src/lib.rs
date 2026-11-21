//! JSON-RPC 2.0 dispatch. Transport and domain errors never enter model messages.
mod agent_catalog;
mod agent_login;
mod approvals;
mod memory_embeddings;
mod scheduler;
mod terminal_subscriptions;

pub use memory_embeddings::{MemoryEmbeddingConfig, MemoryEmbeddingProvider};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use fliuno_core::{
    Error, EventReceiver, EventScope, EventWatermarkReceiver, Kernel, ProgressReceiver,
    TaskSummaryStart,
};
use fliuno_protocol::*;
use scheduler::TaskScheduler;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use std::{
    collections::HashSet,
    sync::{Arc, Mutex, MutexGuard},
};

const MAX_BATCH_ITEMS: usize = 128;

pub struct RpcServer {
    memory_embeddings: Option<memory_embeddings::Service>,
    kernel: Arc<Mutex<Kernel>>,
    scheduler: TaskScheduler,
    agent_catalog: Option<agent_catalog::Catalog>,
    agent_terminal_logins: Option<agent_login::Service>,
    event_receiver: Option<(u64, EventReceiver)>,
    watermark_receiver: Option<(u64, EventWatermarkReceiver)>,
    event_scope: EventScope,
    watermark_only: bool,
    event_stream_gap: bool,
    progress_receiver: Option<ProgressReceiver>,
    progress_stream_gap: bool,
    initialized: bool,
    provider_name: String,
    terminals: Option<fliuno_terminal::Service>,
    terminal_subscriptions: terminal_subscriptions::Subscriptions,
}

enum EventDelivery {
    Events(EventReceiver),
    Watermarks(EventWatermarkReceiver),
}

#[derive(Debug)]
struct RpcError {
    code: i32,
    message: String,
}

impl From<Error> for RpcError {
    fn from(error: Error) -> Self {
        let code = match &error {
            Error::Invalid(_) => -32602,
            Error::NotFound(_) => -32004,
            Error::Conflict(_) => -32009,
            _ => -32603,
        };
        Self {
            code,
            message: error.to_string(),
        }
    }
}

fn error(id: Value, code: i32, message: impl Into<String>) -> Value {
    json!({"jsonrpc":"2.0", "id":id, "error":{"code":code,"message":message.into()}})
}

fn decode<T: DeserializeOwned>(params: Value) -> Result<T, RpcError> {
    if !params.is_object() {
        return Err(RpcError {
            code: -32602,
            message: "named object parameters required".into(),
        });
    }
    serde_json::from_value(params).map_err(|e| RpcError {
        code: -32602,
        message: e.to_string(),
    })
}

impl RpcServer {
    pub fn new(kernel: Kernel) -> Self {
        Self::with_provider(kernel, "demo".into())
    }

    pub fn with_provider(kernel: Kernel, provider_name: String) -> Self {
        Self::with_terminals(kernel, provider_name, None)
    }

    pub fn with_terminals(
        mut kernel: Kernel,
        provider_name: String,
        terminals: Option<fliuno_terminal::Service>,
    ) -> Self {
        let agent_catalog = kernel
            .agent_session_listing_supported()
            .then(agent_catalog::Catalog::new);
        let event_receiver = kernel
            .subscribe_events()
            .expect("cannot subscribe app-server to kernel events");
        let progress_receiver = kernel
            .subscribe_model_progress()
            .expect("cannot subscribe app-server to model progress");
        let startup_run_tasks = match kernel.advance_recoverable_runs() {
            Ok(tasks) => tasks,
            Err(error) => {
                eprintln!("fliuno: cannot recover task runs: {error}");
                Vec::new()
            }
        };
        let kernel = Arc::new(Mutex::new(kernel));
        let scheduler = TaskScheduler::new(Arc::clone(&kernel));
        let server = Self {
            memory_embeddings: None,
            kernel,
            scheduler,
            agent_catalog,
            agent_terminal_logins: None,
            event_receiver: Some((0, event_receiver)),
            watermark_receiver: None,
            event_scope: EventScope::All,
            watermark_only: false,
            event_stream_gap: false,
            progress_receiver: Some(progress_receiver),
            progress_stream_gap: false,
            initialized: false,
            provider_name,
            terminals,
            terminal_subscriptions: terminal_subscriptions::Subscriptions::new(),
        };
        if let Err(error) = server.schedule_run_tasks(startup_run_tasks) {
            eprintln!("fliuno: cannot schedule recovered task runs: {error:?}");
        }
        server
    }

    /// Opt in only for the application's direct local stdio connection.
    pub fn enable_local_agent_terminal_login(
        &mut self,
        registrations: fliuno_agents::TerminalLogins,
    ) {
        if !registrations.is_empty() {
            self.agent_terminal_logins = Some(agent_login::Service::new(registrations));
        }
    }

    pub fn enable_memory_embeddings(
        &mut self,
        provider: MemoryEmbeddingProvider,
    ) -> std::io::Result<()> {
        self.memory_embeddings = Some(memory_embeddings::Service::start(
            provider,
            Arc::clone(&self.kernel),
        )?);
        Ok(())
    }

    pub fn take_event_receiver(&mut self) -> EventReceiver {
        self.take_pending_event_receiver()
            .expect("app-server event receiver was already taken")
    }

    pub fn take_pending_event_receiver(&mut self) -> Option<EventReceiver> {
        self.take_pending_event_subscription()
            .map(|(_, receiver)| receiver)
    }

    pub fn take_pending_event_subscription(&mut self) -> Option<(u64, EventReceiver)> {
        self.event_receiver.take()
    }

    pub fn take_pending_watermark_subscription(&mut self) -> Option<(u64, EventWatermarkReceiver)> {
        self.watermark_receiver.take()
    }

    fn replace_event_subscription(
        &mut self,
        scope: EventScope,
        after: Option<u64>,
        watermark_only: bool,
    ) -> Result<EventSubscription, RpcError> {
        let (through_sequence, delivery) = {
            // Committing workers hold this lock. The watermark and
            // subscription therefore cover adjacent event ranges.
            let kernel = self.kernel()?;
            let through_sequence = kernel.event_watermark()?;
            if after.is_some_and(|cursor| cursor > through_sequence) {
                return Err(Error::Conflict(
                    "event cursor is ahead of the database watermark".into(),
                )
                .into());
            }
            let delivery = if watermark_only {
                EventDelivery::Watermarks(kernel.subscribe_event_watermarks(scope.clone())?)
            } else {
                EventDelivery::Events(match &scope {
                    EventScope::Task(id) => kernel.subscribe_task_events(id)?,
                    EventScope::Session(id) => kernel.subscribe_session_events(id)?,
                    EventScope::Workspace(id) => kernel.subscribe_workspace_events(id)?,
                    EventScope::All => kernel.subscribe_events()?,
                })
            };
            (through_sequence, delivery)
        };
        self.install_event_delivery(scope, through_sequence, delivery);
        Ok(EventSubscription { through_sequence })
    }

    fn subscribe_work(&mut self, request: &WorkSnapshotParams) -> Result<WorkSnapshot, RpcError> {
        let (snapshot, scope, receiver) = {
            let kernel = self.kernel()?;
            // Read first: on a rejected snapshot the current subscription stays intact.
            // Workers cannot commit between this snapshot and registering its receiver.
            let snapshot = kernel.work_snapshot(request)?;
            let scope = match (&request.session_id, &request.workspace_id) {
                (Some(id), _) => EventScope::Session(id.clone()),
                (None, Some(id)) => EventScope::Workspace(id.clone()),
                _ => EventScope::All,
            };
            let receiver = kernel.subscribe_event_watermarks(scope.clone())?;
            (snapshot, scope, receiver)
        };
        self.install_event_delivery(
            scope,
            snapshot.approvals.snapshot.through_sequence,
            EventDelivery::Watermarks(receiver),
        );
        Ok(snapshot)
    }

    fn subscribe_history(
        &mut self,
        request: SessionHistorySubscribeParams,
    ) -> Result<SessionHistoryListPage, RpcError> {
        let scope = EventScope::Session(request.session_id.clone());
        let (page, receiver) = {
            // Keep the commit lock until the receiver is registered. Failed
            // history requests leave the previous subscription in place.
            let kernel = self.kernel()?;
            let page = kernel.list_session_history(&request.into())?;
            let receiver = kernel.subscribe_event_watermarks(scope.clone())?;
            (page, receiver)
        };
        self.install_event_delivery(
            scope,
            page.through_sequence,
            EventDelivery::Watermarks(receiver),
        );
        Ok(page)
    }

    fn subscribe_artifacts(
        &mut self,
        request: ArtifactCatalogParams,
    ) -> Result<ArtifactSubscriptionSnapshot, RpcError> {
        let scope = match &request.scope {
            ArtifactCatalogScope::Session { session_id } => EventScope::Session(session_id.clone()),
            ArtifactCatalogScope::Workspace { workspace_id } => {
                EventScope::Workspace(workspace_id.clone())
            }
        };
        let (catalog, event_through_sequence, receiver) = {
            // Catalog rows and their ArtifactCreated events commit together.
            // Keep this lock across both reads and the receiver registration.
            let kernel = self.kernel()?;
            let catalog = kernel.list_artifact_snapshot(request.into())?;
            let event_through_sequence = kernel.event_watermark()?;
            let receiver = kernel.subscribe_event_watermarks(scope.clone())?;
            (catalog, event_through_sequence, receiver)
        };
        self.install_event_delivery(
            scope,
            event_through_sequence,
            EventDelivery::Watermarks(receiver),
        );
        Ok(ArtifactSubscriptionSnapshot {
            catalog,
            event_through_sequence,
        })
    }

    fn install_event_delivery(&mut self, scope: EventScope, through: u64, delivery: EventDelivery) {
        (
            self.event_receiver,
            self.watermark_receiver,
            self.watermark_only,
        ) = match delivery {
            EventDelivery::Events(receiver) => (Some((through, receiver)), None, false),
            EventDelivery::Watermarks(receiver) => (None, Some((through, receiver)), true),
        };
        self.event_scope = scope;
        self.event_stream_gap = false;
    }

    pub fn mark_event_stream_gap(&mut self) {
        self.event_stream_gap = true;
    }

    pub fn take_progress_receiver(&mut self) -> ProgressReceiver {
        self.take_pending_progress_receiver()
            .expect("app-server progress receiver was already taken")
    }

    pub fn take_pending_progress_receiver(&mut self) -> Option<ProgressReceiver> {
        self.progress_receiver.take()
    }

    pub fn mark_progress_stream_gap(&mut self) {
        self.progress_stream_gap = true;
    }

    pub fn take_terminal_notification_receiver(&mut self) -> crossbeam_channel::Receiver<()> {
        self.terminal_subscriptions.take_receiver()
    }

    pub fn next_terminal_notification(&self) -> Option<Value> {
        self.terminal_subscriptions
            .next()
            .map(|value| value.into_rpc())
    }

    fn kernel(&self) -> Result<MutexGuard<'_, Kernel>, RpcError> {
        self.kernel.lock().map_err(|_| RpcError {
            code: -32603,
            message: "kernel lock is poisoned".into(),
        })
    }

    fn retrieve_task_memory(
        &self,
        params: &TaskStartParams,
    ) -> Result<memory_embeddings::Retrieved, RpcError> {
        let Some(service) = &self.memory_embeddings else {
            return Ok(memory_embeddings::Retrieved::Keyword);
        };
        let workspace = self.kernel()?.task_memory_workspace(params)?;
        Ok(
            workspace.map_or(memory_embeddings::Retrieved::Keyword, |workspace| {
                service.query(workspace, &params.input)
            }),
        )
    }

    /// Call with the kernel lock held after commit so a worker cannot record
    /// its cancellation outcome ahead of the user's durable transition.
    fn signal_committed_cancellations<'a>(&self, ids: impl IntoIterator<Item = &'a str>) {
        if let Err(error) = self.scheduler.cancel_all(ids) {
            // The state is already durable. Keep the receipt and diagnose
            // the failed background signal separately.
            eprintln!("fliuno: signalling committed cancellation: {error}");
        }
    }

    fn schedule_or_fail(&self, task: Task) -> Result<Task, RpcError> {
        if task.status != TaskStatus::Running {
            return Ok(task);
        }
        let scheduled = match task.backend {
            TaskBackend::Model => self.scheduler.schedule(&task.id),
            TaskBackend::ExternalAgent { .. } => self.scheduler.schedule_agent(&task.id),
        };
        match scheduled {
            Ok(()) => Ok(task),
            Err(message) => Ok(self.kernel()?.fail_task(&task.id, message.into())?),
        }
    }

    fn advance_run_with_deadline(&self, run_id: &str) -> Result<(Run, Vec<Task>), RpcError> {
        let mut kernel = self.kernel()?;
        if let Some(ids) = kernel.expire_run(run_id)? {
            self.signal_committed_cancellations(ids.iter().map(String::as_str));
        }
        Ok(kernel.advance_run(run_id)?)
    }

    fn schedule_run_tasks(&self, tasks: Vec<Task>) -> Result<(), RpcError> {
        let mut pending = tasks;
        let mut scheduled = HashSet::new();
        while let Some(task) = pending.pop() {
            let task_id = task.id.clone();
            if !scheduled.insert(task_id.clone()) {
                continue;
            }
            self.schedule_or_fail(task)?;
            pending.extend(self.kernel()?.advance_runs_for_task(&task_id)?);
        }
        Ok(())
    }

    /// One JSON value per line; a JSON-RPC batch is a single value.
    /// Notifications are processed without generating a response.
    pub fn handle_line(&mut self, line: &str) -> Option<Value> {
        match serde_json::from_str::<Value>(line) {
            Err(_) => Some(error(Value::Null, -32700, "parse error")),
            Ok(Value::Array(items)) => {
                if items.is_empty() {
                    return Some(error(Value::Null, -32600, "empty batch"));
                }
                if items.len() > MAX_BATCH_ITEMS {
                    return Some(error(
                        Value::Null,
                        -32600,
                        format!("batch exceeds {MAX_BATCH_ITEMS} items"),
                    ));
                }
                let responses: Vec<Value> = items
                    .into_iter()
                    .filter_map(|item| self.handle_request(item))
                    .collect();
                if responses.is_empty() {
                    None
                } else {
                    Some(Value::Array(responses))
                }
            }
            Ok(value) => self.handle_request(value),
        }
    }

    fn handle_request(&mut self, value: Value) -> Option<Value> {
        let Value::Object(mut request) = value else {
            return Some(error(Value::Null, -32600, "invalid request"));
        };
        let Some(Value::String(method)) = request.remove("method") else {
            return Some(error(Value::Null, -32600, "invalid request"));
        };
        let id = request.remove("id");
        if request.get("jsonrpc").and_then(Value::as_str) != Some("2.0")
            || id
                .as_ref()
                .is_some_and(|id| !(id.is_null() || id.is_string() || id.is_number()))
        {
            return Some(error(Value::Null, -32600, "invalid request"));
        }
        let notification = id.is_none();
        if notification && method.starts_with("agents/terminalLogin/") {
            return None;
        }
        let id = id.unwrap_or(Value::Null);
        let params = request.remove("params").unwrap_or_else(|| json!({}));
        let result = if !params.is_object() && !params.is_array() {
            Err(RpcError {
                code: -32602,
                message: "params must be an object or array".into(),
            })
        } else {
            self.dispatch(&method, params)
        };
        if notification {
            return None;
        }
        Some(match result {
            Ok(result) => json!({"jsonrpc":"2.0", "id":id, "result":result}),
            Err(err) => error(id, err.code, err.message),
        })
    }

    fn dispatch(&mut self, method: &str, params: Value) -> Result<Value, RpcError> {
        if method == "initialize" {
            let params: InitializeParams = decode(params)?;
            if self.initialized {
                return Err(RpcError {
                    code: -32009,
                    message: "already initialized".into(),
                });
            }
            if params.protocol_version != PROTOCOL_VERSION {
                return Err(RpcError {
                    code: -32010,
                    message: format!("supported protocol version: {PROTOCOL_VERSION}"),
                });
            }
            if params.client_name.trim().is_empty() {
                return Err(RpcError {
                    code: -32602,
                    message: "clientName is required".into(),
                });
            }
            let external_tools = self.kernel()?.tool_specs().iter().any(|s| {
                matches!(
                    s.effect,
                    fliuno_core::ToolEffect::External
                        | fliuno_core::ToolEffect::ProcessExecution
                        | fliuno_core::ToolEffect::TerminalControl
                )
            });
            let process_execution = self
                .kernel()?
                .tool_specs()
                .iter()
                .any(|spec| matches!(spec.effect, fliuno_core::ToolEffect::ProcessExecution));
            self.initialized = true;
            let mut capabilities = json!({
                "eventReplay":true, "approvals":true, "artifacts":true,
                "artifactSummaries":true,
                "registrationAuthority":true,
                "eventNotifications":true, "globalEventReplay":true,
                "eventReplayTargetBytes":fliuno_core::MAX_EVENT_TARGET_BYTES,
                "eventResubscribe":true,
                "sessionEventReplay":true,
                "maxBatchItems":MAX_BATCH_ITEMS,
                "provider":self.provider_name, "liveModels":self.provider_name != "demo",
                "externalTools":external_tools, "backgroundTasks":true,
                "externalAgents":!self.kernel()?.agent_specs().is_empty(),
                "remoteTaskChecks":self.kernel()?.remote_task_check_supported(),
                "remoteTaskContinuation":self.kernel()?.remote_continuation_supported(),
                "sessionContext":true,
                "sessionTitles":true,
                "sessionCreateIdempotency":true,
                "taskSummaries":true,
                "contextTaskSelection":true,
                "contextFitRecent":true,
                "processExecution":process_execution,
                "modelProgressNotifications":self.kernel()?.model_progress_supported(),
                "modelProgressResubscribe":self.kernel()?.model_progress_supported(),
                "modelBudget":self.kernel()?.configured_model_budget(),
                "terminalSessions":self.terminals.is_some(),
                "terminalOutputSubscriptions":self.terminals.is_some(),
                "terminalOutputTail":self.terminals.is_some(),
                "terminalArchiving":self.terminals.is_some(),
                "terminalBatchArchiving":self.terminals.is_some(),
                "maxTerminalArchiveBatch":MAX_TERMINAL_ARCHIVE_BATCH,
                "maxTerminalSubscriptions":terminal_subscriptions::MAX_SUBSCRIPTIONS,
                "maxContextTurns":fliuno_core::MAX_CONTEXT_TURNS,
                "maxContextCandidates":fliuno_core::MAX_CONTEXT_CANDIDATES,
                "maxContextBytes":fliuno_core::MAX_CONTEXT_BYTES,
                "taskCancellation":true,
                "concurrentExecution":scheduler::WORKER_COUNT > 1,
                "maxConcurrentExecutions":scheduler::WORKER_COUNT
            });
            capabilities["modelBudgetRejectionEvents"] = json!(true);
            capabilities["durableTaskRuns"] = json!(self.kernel()?.durable_runs_supported());
            capabilities["maxRunNodes"] = json!(MAX_RUN_NODES);
            capabilities["maxRunConcurrency"] = json!(MAX_RUN_CONCURRENCY);
            capabilities["maxRunTimeoutMs"] = json!(MAX_RUN_TIMEOUT_MS);
            capabilities["modelCapabilities"] =
                json!(self.kernel()?.configured_model_capabilities());
            capabilities["taskStartSummary"] = json!(true);
            capabilities["taskRespondToolInput"] = json!(true);
            capabilities["taskImageInput"] = json!(true);
            capabilities["taskPdfInput"] = json!(true);
            capabilities["taskArtifactInput"] = json!(true);
            capabilities["taskTextArtifactInput"] = json!(true);
            capabilities["maxTaskModelSteps"] = json!(MAX_TASK_MODEL_STEPS);
            capabilities["taskResumeStatus"] = json!(true);
            capabilities["taskResumeStatusBatch"] = json!(true);
            capabilities["maxTaskResumeStatusBatch"] = json!(MAX_TASK_SUMMARY_BATCH);
            capabilities["taskCancelSummary"] = json!(true);
            capabilities["taskCancelBatch"] = json!(true);
            capabilities["maxTaskCancelBatch"] = json!(MAX_TASK_CANCEL_BATCH);
            capabilities["taskSummaryByRequest"] = json!(true);
            capabilities["taskSummaryByRequestBatch"] = json!(true);
            capabilities["maxTaskSummaryByRequestBatch"] = json!(MAX_TASK_SUMMARY_BATCH);
            capabilities["sessionHistoryList"] = json!(true);
            capabilities["sessionHistoryAfterSequence"] = json!(true);
            capabilities["sessionHistorySubscription"] = json!(true);
            capabilities["sessionHistoryGet"] = json!(true);
            capabilities["maxSessionHistoryBytes"] = json!(MAX_SESSION_HISTORY_BYTES);
            capabilities["maxSessionHistoryEntries"] = json!(MAX_SESSION_HISTORY_ENTRIES);
            capabilities["sessionHistorySearch"] = json!(true);
            capabilities["workspaceHistorySearch"] = json!(true);
            capabilities["historySearchAfterSequence"] = json!(true);
            capabilities["maxHistorySearchScanLimit"] = json!(MAX_HISTORY_SEARCH_SCAN_LIMIT);
            capabilities["contextMatchRecent"] = json!(true);
            capabilities["maxContextScanLimit"] = json!(MAX_CONTEXT_SCAN_LIMIT);
            capabilities["artifactMediaTypeFilter"] = json!(true);
            capabilities["remoteTaskWatch"] = json!(self.kernel()?.remote_task_watch_supported());
            capabilities["remoteTaskCheckLookup"] = json!(true);
            capabilities["agentCapabilitiesById"] = json!(true);
            let check_workers = self.scheduler.remote_check_workers();
            capabilities["maxConcurrentRemoteTaskChecks"] = json!(check_workers);
            capabilities["maxQueuedRemoteTaskChecks"] =
                json!(check_workers * scheduler::REMOTE_CHECK_QUEUE_CAPACITY);
            capabilities["workspaceEventReplay"] = json!(true);
            capabilities["eventBySequence"] = json!(true);
            capabilities["eventSubscriptionScope"] = json!(true);
            capabilities["eventSubscriptionCursor"] = json!(true);
            capabilities["eventWatermarkNotifications"] = json!(true);
            capabilities["coalescedEventWatermarks"] = json!(true);
            capabilities["taskEventWatermarks"] = json!(true);
            capabilities["eventWatermarkQuery"] = json!(true);
            capabilities["workspaceEventSubscription"] = json!(true);
            capabilities["taskEventSubscription"] = json!(true);
            capabilities["contextConversationDetail"] = json!(true);
            capabilities["contextAtSequence"] = json!(true);
            capabilities["contextAfterSequence"] = json!(true);
            capabilities["contextThroughSequence"] = json!(true);
            capabilities["taskListTargetBytes"] = json!(fliuno_core::MAX_TASK_LIST_TARGET_BYTES);
            capabilities["taskSummaryListTargetBytes"] =
                json!(fliuno_core::MAX_TASK_LIST_TARGET_BYTES);
            capabilities["taskSummaryById"] = json!(true);
            capabilities["taskSummaryBatch"] = json!(true);
            capabilities["taskChangeReplay"] = json!(true);
            capabilities["taskSummaryChangeReplay"] = json!(true);
            capabilities["taskSummaryRevisionReplay"] = json!(true);
            capabilities["latestTaskSummaryRevisionReplay"] = json!(true);
            capabilities["maxTaskSummaryChangeEvents"] = json!(MAX_TASK_SUMMARY_CHANGE_EVENTS);
            capabilities["maxTaskSummaryBatch"] = json!(MAX_TASK_SUMMARY_BATCH);
            capabilities["workSnapshot"] = json!(true);
            capabilities["workSessionCounts"] = json!(true);
            capabilities["maxWorkSessionCountBatch"] = json!(MAX_WORK_SESSION_COUNT_BATCH);
            capabilities["workSubscription"] = json!(true);
            capabilities["workRecentTasks"] = json!(true);
            capabilities["taskSummaryStatusFilter"] = json!(true);
            capabilities["workspaceTaskSummaries"] = json!(true);
            capabilities["workspaceTaskSummaryOrdering"] = json!(true);
            capabilities["sessionTaskSummaryOrdering"] = json!(true);
            capabilities["workspaceById"] = json!(true);
            capabilities["workspaceProfiles"] = json!(true);
            capabilities["workspaceMemory"] = json!(true);
            capabilities["workspaceMemoryAudit"] = json!(true);
            capabilities["workspaceMemorySemanticIndex"] = json!(true);
            capabilities["taskMemoryOptOut"] = json!(true);
            capabilities["taskMemoryBudgetReduction"] = json!(true);
            capabilities["taskMemoryHybridRetrieval"] = json!(self.memory_embeddings.is_some());
            capabilities["maxWorkspaceMemories"] = json!(4096);
            capabilities["maxWorkspaceMemoryContentBytes"] =
                json!(MAX_WORKSPACE_MEMORY_CONTENT_BYTES);
            capabilities["modelUsageEvents"] = json!(true);
            capabilities["modelUsageEventReplay"] = json!(true);
            capabilities["modelUsageScopedReplay"] = json!(true);
            capabilities["modelUsageSummary"] = json!(true);
            capabilities["modelUsageSummaryBatch"] = json!(true);
            capabilities["modelUsageRangeSummary"] = json!(true);
            capabilities["maxModelUsageRangeReports"] = json!(MAX_MODEL_USAGE_RANGE_REPORTS);
            capabilities["maxModelUsageSummaryBatch"] = json!(MAX_MODEL_USAGE_SUMMARY_BATCH);
            capabilities["maxWorkspaceTitleChars"] = json!(MAX_WORKSPACE_TITLE_CHARS);
            capabilities["agentSessionContinuation"] =
                json!(self.kernel()?.agent_session_continuation_supported());
            capabilities["agentAuthenticationState"] = json!(true);
            if self.agent_terminal_logins.is_some() {
                capabilities["agentTerminalLogin"] = json!(true);
            }
            if self.agent_catalog.is_some() {
                capabilities["agentSessionListing"] = json!(true);
                capabilities["maxQueuedAgentSessionLists"] = json!(agent_catalog::QUEUE_CAPACITY);
                capabilities["maxRetainedAgentSessionLists"] = json!(agent_catalog::MAX_RETAINED);
            }
            capabilities["pendingApprovalInbox"] = json!(true);
            capabilities["pendingApprovalInboxSnapshot"] = json!(true);
            capabilities["pendingApprovalListTargetBytes"] =
                json!(fliuno_core::MAX_APPROVAL_LIST_TARGET_BYTES);
            capabilities["pendingApprovalDetails"] = json!(true);
            capabilities["pendingApprovalDetailsBatch"] = json!(true);
            capabilities["maxApprovalDetailsBatch"] = json!(MAX_APPROVAL_DETAILS_BATCH);
            capabilities["maxApprovalDetailsBatchBytes"] = json!(MAX_APPROVAL_DETAILS_BATCH_BYTES);
            capabilities["approvalDecisionLookup"] = json!(true);
            capabilities["approvalDecisionBatch"] = json!(true);
            capabilities["maxApprovalDecisionBatch"] = json!(MAX_APPROVAL_DECISION_BATCH);
            capabilities["approvalRespondDecision"] = json!(true);
            capabilities["approvalResumeRun"] = json!(true);
            capabilities["approvalRunWindow"] = json!(true);
            capabilities["approvalRunReplay"] = json!(true);
            capabilities["approvalRunPending"] = json!(true);
            capabilities["sessionArchiving"] = json!(true);
            capabilities["sessionListOrdering"] = json!(true);
            capabilities["sessionArchiveBatch"] = json!(true);
            capabilities["maxSessionArchiveBatch"] =
                json!(fliuno_protocol::MAX_SESSION_ARCHIVE_BATCH);
            capabilities["artifactByteReads"] = json!(true);
            capabilities["artifactDownloadStart"] = json!(true);
            capabilities["artifactDownloadStartBatch"] = json!(true);
            capabilities["maxArtifactDownloadBatch"] = json!(MAX_ARTIFACT_DOWNLOAD_BATCH);
            capabilities["maxArtifactDownloadBatchBytes"] =
                json!(MAX_ARTIFACT_DOWNLOAD_BATCH_BYTES);
            capabilities["storedArtifactDigests"] = json!(true);
            capabilities["artifactDigestBatch"] = json!(true);
            capabilities["maxArtifactDigestBatch"] = json!(MAX_ARTIFACT_DIGEST_BATCH);
            capabilities["binaryArtifacts"] = json!(true);
            capabilities["maxBinaryArtifactBytes"] = json!(MAX_BINARY_ARTIFACT_BYTES);
            capabilities["artifactCatalogSnapshots"] = json!(true);
            capabilities["artifactCatalogSubscription"] = json!(true);
            capabilities["artifactSummaryById"] = json!(true);
            capabilities["scopedArtifactSummaries"] = json!(true);
            capabilities["maxArtifactReadBytes"] = json!(MAX_ARTIFACT_READ_BYTES);
            capabilities["maxArtifactDigestBytes"] = json!(MAX_ARTIFACT_DIGEST_BYTES);
            return Ok(json!({
                "protocolVersion": PROTOCOL_VERSION,
                "serverName": "fliuno-app-server",
                "serverVersion": env!("CARGO_PKG_VERSION"),
                "capabilities": capabilities,
            }));
        }
        if !self.initialized {
            return Err(RpcError {
                code: -32002,
                message: "initialize first".into(),
            });
        }
        Ok(match method {
            "terminal/archiveBatch" => {
                let p: TerminalArchiveBatchParams = decode(params)?;
                json!(
                    self.terminal_service()?
                        .archive_batch(&p.session_id, &p.terminal_ids)
                        .map_err(terminal_error)?
                )
            }
            "terminal/archive" => {
                let p: TerminalArchiveParams = decode(params)?;
                json!(
                    self.terminal_service()?
                        .archive(&p.session_id, &p.terminal_id)
                        .map_err(|error| terminal_error(error.to_string()))?
                )
            }
            "terminal/archives" => {
                let p: TerminalArchivesParams = decode(params)?;
                json!(
                    self.terminal_service()?
                        .archives(&p.session_id, p.after, p.limit)
                        .map_err(terminal_error)?
                )
            }
            "terminal/subscribe" => {
                let p: TerminalSubscribeParams = decode(params)?;
                let service = self.terminal_service()?.clone();
                json!(
                    self.terminal_subscriptions
                        .start(&service, p)
                        .map_err(terminal_error)?
                )
            }
            "terminal/unsubscribe" => {
                let p: TerminalUnsubscribeParams = decode(params)?;
                json!({"removed":self.terminal_subscriptions.stop(&p.subscription_id).map_err(terminal_error)?})
            }
            "terminal/list" => {
                let p: TerminalListParams = decode(params)?;
                let runtime = terminal_runtime()?;
                json!(
                    runtime
                        .block_on(self.terminal_service()?.list(&p.session_id))
                        .map_err(terminal_error)?
                )
            }
            "terminal/get" => {
                let p: TerminalGetParams = decode(params)?;
                let runtime = terminal_runtime()?;
                json!(
                    runtime
                        .block_on(
                            self.terminal_service()?
                                .status(&p.session_id, &p.terminal_id)
                        )
                        .map_err(terminal_error)?
                )
            }
            "terminal/output" => {
                let p: TerminalOutputParams = decode(params)?;
                let runtime = terminal_runtime()?;
                json!(
                    runtime
                        .block_on(self.terminal_service()?.output(
                            &p.session_id,
                            &p.terminal_id,
                            p.after,
                            p.limit
                        ))
                        .map_err(terminal_error)?
                )
            }
            "terminal/tail" => {
                let p: TerminalTailParams = decode(params)?;
                json!(
                    self.terminal_service()?
                        .tail(&p.session_id, &p.terminal_id, p.limit)
                        .map_err(terminal_error)?
                )
            }
            "work/getSnapshot" => {
                let p: WorkSnapshotParams = decode(params)?;
                json!(self.kernel()?.work_snapshot(&p)?)
            }
            "work/getSessionCounts" => {
                let p: WorkSessionCountsParams = decode(params)?;
                json!(self.kernel()?.work_session_counts(&p)?)
            }
            "work/subscribe" => {
                let p: WorkSnapshotParams = decode(params)?;
                json!(self.subscribe_work(&p)?)
            }
            "workspace/open" => {
                let p: WorkspaceOpenParams = decode(params)?;
                json!(self.kernel()?.open_workspace(&p.root)?)
            }
            "workspace/get" => {
                let p: WorkspaceIdParams = decode(params)?;
                json!(self.kernel()?.get_workspace(&p.workspace_id)?)
            }
            "workspace/getProfile" => {
                let p: WorkspaceIdParams = decode(params)?;
                json!(self.kernel()?.get_workspace_profile(&p.workspace_id)?)
            }
            "workspace/listProfiles" => {
                let p: ListParams = decode(params)?;
                json!(self.kernel()?.list_workspace_profiles(p.after, p.limit)?)
            }
            "workspace/setTitle" => {
                let p: WorkspaceSetTitleParams = decode(params)?;
                json!(
                    self.kernel()?
                        .set_workspace_title(&p.workspace_id, &p.title)?
                )
            }
            "workspace/memory/put" => {
                let p: WorkspaceMemoryPutParams = decode(params)?;
                json!(self.kernel()?.put_workspace_memory(&p)?)
            }
            "workspace/memory/get" => {
                let p: WorkspaceMemoryGetParams = decode(params)?;
                json!(self.kernel()?.get_workspace_memory(&p)?)
            }
            "workspace/memory/list" => {
                let p: WorkspaceMemoryListParams = decode(params)?;
                json!(self.kernel()?.list_workspace_memories(&p)?)
            }
            "workspace/memory/search" => {
                let p: WorkspaceMemorySearchParams = decode(params)?;
                json!(self.kernel()?.search_workspace_memories(&p)?)
            }
            "workspace/memory/indexVectors" => {
                let p: WorkspaceMemoryIndexParams = decode(params)?;
                json!({"indexed":self.kernel()?.index_workspace_memory_vectors(&p)?})
            }
            "workspace/memory/semanticSearch" => {
                let p: WorkspaceMemorySemanticSearchParams = decode(params)?;
                json!(self.kernel()?.semantic_search_workspace_memories(&p)?)
            }
            "workspace/memory/delete" => {
                let p: WorkspaceMemoryDeleteParams = decode(params)?;
                json!(self.kernel()?.delete_workspace_memory(&p)?)
            }
            "workspace/memory/listAudit" => {
                let p: WorkspaceMemoryAuditListParams = decode(params)?;
                json!(self.kernel()?.list_workspace_memory_audit(&p)?)
            }
            "workspace/list" => {
                let p: ListParams = decode(params)?;
                json!(self.kernel()?.list_workspaces(p.after, p.limit)?)
            }
            "session/list" => {
                let p: SessionListParams = decode(params)?;
                json!(self.kernel()?.list_sessions_ordered(
                    &p.workspace_id,
                    p.after,
                    p.limit,
                    p.archived,
                    p.order.unwrap_or_default(),
                )?)
            }
            "task/list" => {
                let p: TaskListParams = decode(params)?;
                json!(self.kernel()?.list_tasks_with_target(
                    &p.session_id,
                    p.after,
                    p.limit,
                    p.target_bytes,
                )?)
            }
            "task/respondToolInput" => {
                let p: ToolInputRespondParams = decode(params)?;
                json!(self.respond_tool_input(p)?)
            }
            "task/listSummaries" => {
                let p: TaskSummaryListParams = decode(params)?;
                json!(self.kernel()?.list_task_summaries_ordered(
                    &p.session_id,
                    p.after,
                    p.limit,
                    p.status,
                    p.target_bytes,
                    p.order.unwrap_or_default(),
                )?)
            }
            "task/listWorkspaceSummaries" => {
                let p: WorkspaceTaskSummaryListParams = decode(params)?;
                json!(self.kernel()?.list_workspace_task_summaries_ordered(
                    &p.workspace_id,
                    p.after,
                    p.limit,
                    p.status,
                    p.target_bytes,
                    p.order.unwrap_or_default(),
                )?)
            }
            "session/create" => {
                let p: SessionCreateParams = decode(params)?;
                json!(self.kernel()?.create_session_with_request(
                    &p.workspace_id,
                    p.title.as_deref(),
                    p.client_request_id.as_deref()
                )?)
            }
            "session/get" => {
                let p: SessionIdParams = decode(params)?;
                json!(self.kernel()?.get_session(&p.session_id)?)
            }
            "session/rename" => {
                let p: SessionRenameParams = decode(params)?;
                json!(self.kernel()?.rename_session(&p.session_id, &p.title)?)
            }
            "session/setArchived" => {
                let p: SessionSetArchivedParams = decode(params)?;
                json!(
                    self.kernel()?
                        .set_session_archived(&p.session_id, p.archived)?
                )
            }
            "session/setArchivedBatch" => {
                let p: SessionArchiveBatchParams = decode(params)?;
                json!(SessionArchiveBatch {
                    sessions: self
                        .kernel()?
                        .set_sessions_archived(&p.session_ids, p.archived)?,
                })
            }
            "session/listHistory" => {
                let p: SessionHistoryListParams = decode(params)?;
                json!(self.kernel()?.list_session_history(&p)?)
            }
            "session/subscribeHistory" => {
                let p: SessionHistorySubscribeParams = decode(params)?;
                json!(self.subscribe_history(p)?)
            }
            "session/getHistoryTurn" => {
                let p: SessionHistoryGetParams = decode(params)?;
                json!(self.kernel()?.get_session_history_turn(&p)?)
            }
            "session/searchHistory" => {
                let p: SessionHistorySearchParams = decode(params)?;
                json!(self.kernel()?.search_session_history(p)?)
            }
            "workspace/searchHistory" => {
                let p: WorkspaceHistorySearchParams = decode(params)?;
                json!(self.kernel()?.search_workspace_history(p)?)
            }
            "session/context" => {
                let p: SessionContextParams = decode(params)?;
                json!(self.kernel()?.session_context_through(
                    &p.session_id,
                    p.options,
                    p.through_sequence,
                )?)
            }
            "task/startSummary" => {
                let p: TaskStartParams = decode(params)?;
                let memory = self.retrieve_task_memory(&p)?;
                let started = self
                    .kernel()?
                    .create_summary_with_memory(p, memory.query())?;
                let summary = match started {
                    TaskSummaryStart::Existing(summary) => summary,
                    TaskSummaryStart::Created(task) => {
                        let task = self.schedule_or_fail(*task)?;
                        self.kernel()?.get_task_summary(&task.id)?
                    }
                };
                json!(summary)
            }
            "task/start" => {
                let p: TaskStartParams = decode(params)?;
                let memory = self.retrieve_task_memory(&p)?;
                let (task, created) = self.kernel()?.create_task_with_memory(p, memory.query())?;
                let task = if created {
                    self.schedule_or_fail(task)?
                } else {
                    task
                };
                json!(task)
            }
            "run/start" => {
                let p: RunStartParams = decode(params)?;
                let started = self.kernel()?.start_run(p);
                self.scheduler.notify_run_deadline();
                let (run, tasks, _) = started?;
                self.schedule_run_tasks(tasks)?;
                let (run, tasks) = self.advance_run_with_deadline(&run.id)?;
                self.schedule_run_tasks(tasks)?;
                json!(self.kernel()?.get_run(&run.id)?)
            }
            "run/get" => {
                let p: RunIdParams = decode(params)?;
                let (run, tasks) = self.advance_run_with_deadline(&p.run_id)?;
                self.schedule_run_tasks(tasks)?;
                json!(self.kernel()?.get_run(&run.id)?)
            }
            "run/cancel" => {
                let p: RunCancelParams = decode(params)?;
                let mut kernel = self.kernel()?;
                let (run, task_ids) = kernel.cancel_run(&p.run_id)?;
                self.signal_committed_cancellations(task_ids.iter().map(String::as_str));
                json!(run)
            }
            "run/events" => {
                let p: RunEventsParams = decode(params)?;
                json!(self.kernel()?.list_run_events(&p)?)
            }
            "task/getSummaryByRequest" => {
                let p: TaskRequestLookupParams = decode(params)?;
                json!(
                    self.kernel()?
                        .get_task_summary_by_request(&p.session_id, &p.client_request_id)?
                )
            }
            "task/getSummariesByRequest" => {
                let p: TaskRequestSummaryBatchParams = decode(params)?;
                json!(self.kernel()?.get_task_summaries_by_request(&p)?)
            }
            "task/getSummary" => {
                let p: TaskIdParams = decode(params)?;
                json!(self.kernel()?.get_task_summary(&p.task_id)?)
            }
            "task/getSummaries" => {
                let p: TaskSummaryBatchParams = decode(params)?;
                json!(self.kernel()?.get_task_summaries(&p)?)
            }
            "task/get" => {
                let p: TaskIdParams = decode(params)?;
                json!(self.kernel()?.get_task(&p.task_id)?)
            }
            "task/continueRemote" => {
                let p: TaskContinueRemoteParams = decode(params)?;
                let (task, created) = self.kernel()?.create_remote_continuation(
                    &p.task_id,
                    &p.input,
                    &p.client_request_id,
                )?;
                json!(if created {
                    self.schedule_or_fail(task)?
                } else {
                    task
                })
            }
            "task/continueAgentSession" => {
                let p: TaskContinueAgentSessionParams = decode(params)?;
                let (task, created) = self.kernel()?.create_agent_session_continuation(
                    &p.task_id,
                    &p.input,
                    &p.client_request_id,
                )?;
                json!(if created {
                    self.schedule_or_fail(task)?
                } else {
                    task
                })
            }
            "task/checkRemote" => {
                let p: RemoteTaskCheckParams = decode(params)?;
                let check = self
                    .kernel()?
                    .prepare_remote_task_observation(&p.task_id, p.wait_ms)?;
                let accepted = RemoteTaskCheckAccepted {
                    check_id: check.check_id().to_owned(),
                };
                self.scheduler
                    .schedule_remote_check(check)
                    .map_err(|message| Error::Conflict(message.into()))?;
                json!(accepted)
            }
            "task/getRemoteCheck" => {
                let p: RemoteTaskCheckLookupParams = decode(params)?;
                json!(
                    self.kernel()?
                        .get_remote_task_check(&p.task_id, &p.check_id)?
                )
            }
            "task/cancel" | "task/cancelSummary" => {
                let p: TaskIdParams = decode(params)?;
                let mut kernel = self.kernel()?;
                let task = kernel.cancel_task(&p.task_id)?;
                self.signal_committed_cancellations([task.id.as_str()]);
                let followups = kernel.advance_runs_for_task(&task.id)?;
                let result = if method == "task/cancelSummary" {
                    json!(kernel.get_task_summary(&task.id)?)
                } else {
                    json!(task)
                };
                drop(kernel);
                self.schedule_run_tasks(followups)?;
                result
            }
            "task/cancelBatch" => {
                let p: TaskCancelBatchParams = decode(params)?;
                let mut kernel = self.kernel()?;
                let batch = kernel.cancel_task_batch(&p)?;
                self.signal_committed_cancellations(
                    batch
                        .items
                        .iter()
                        .filter(|item| item.task.status == TaskStatus::Cancelled)
                        .map(|item| item.task.id.as_str()),
                );
                let followups = batch
                    .items
                    .iter()
                    .filter(|item| item.task.status == TaskStatus::Cancelled)
                    .try_fold(Vec::new(), |mut tasks, item| {
                        tasks.extend(kernel.advance_runs_for_task(&item.task.id)?);
                        Ok::<_, Error>(tasks)
                    })?;
                drop(kernel);
                self.schedule_run_tasks(followups)?;
                json!(batch)
            }
            "task/getResumeStatus" => {
                let p: TaskIdParams = decode(params)?;
                json!(self.kernel()?.get_task_resume_status(&p.task_id)?)
            }
            "task/getResumeStatuses" => {
                let p: TaskSummaryBatchParams = decode(params)?;
                json!(self.kernel()?.get_task_resume_statuses(&p)?)
            }
            "task/resume" => {
                let p: TaskResumeParams = decode(params)?;
                let task = self
                    .kernel()?
                    .prepare_resume_task(&p.task_id, p.retry_uncertain_model_step)?;
                let task = self.schedule_or_fail(task)?;
                let followups = self.kernel()?.advance_runs_for_task(&task.id)?;
                self.schedule_run_tasks(followups)?;
                json!(task)
            }
            "approval/listPending" => {
                let p: ApprovalListParams = decode(params)?;
                json!(self.kernel()?.list_pending_approvals(p)?)
            }
            "approval/listPendingSnapshot" => {
                let p: ApprovalInboxListParams = decode(params)?;
                json!(self.kernel()?.list_pending_approval_snapshot(&p)?)
            }
            "approval/get" => {
                let p: ApprovalGetParams = decode(params)?;
                json!(
                    self.kernel()?
                        .get_pending_approval(&p.task_id, &p.approval_id)?
                )
            }
            "approval/getBatch" => {
                let p: ApprovalGetBatchParams = decode(params)?;
                json!(self.kernel()?.get_pending_approvals(&p)?)
            }
            "approval/getDecision" => {
                let p: ApprovalGetParams = decode(params)?;
                json!(
                    self.kernel()?
                        .get_approval_decision(&p.task_id, &p.approval_id)?
                )
            }
            "approval/getDecisions" => {
                let p: ApprovalDecisionBatchParams = decode(params)?;
                json!(self.kernel()?.get_approval_decisions(&p)?)
            }
            "approval/respond" => json!(self.respond_approval(decode(params)?)?),
            "approval/respondDecision" => json!(self.respond_approval_decision(decode(params)?)?),
            "approval/resumeRun" => json!(self.resume_approval_run(decode(params)?)?),
            "approval/cancelRun" => json!(self.cancel_approval_run(decode(params)?)?),
            "approval/getRun" => {
                let p: TaskRequestLookupParams = decode(params)?;
                json!(
                    self.kernel()?
                        .get_approval_run(&p.session_id, &p.client_request_id)?
                )
            }
            "approval/getRunWindow" => {
                let p: TaskRequestLookupParams = decode(params)?;
                json!(
                    self.kernel()?
                        .get_approval_run_window(&p.session_id, &p.client_request_id)?
                )
            }
            "approval/getRunReplay" => {
                let p: TaskRequestLookupParams = decode(params)?;
                json!(
                    self.kernel()?
                        .get_approval_run_replay(&p.session_id, &p.client_request_id)?
                )
            }
            "approval/getRunPending" => {
                let p: ApprovalRunPendingParams = decode(params)?;
                json!(self.kernel()?.get_approval_run_pending(&p)?)
            }
            "events/get" => {
                let p: EventGetParams = decode(params)?;
                json!(self.kernel()?.get_event(p.sequence)?)
            }
            "events/list" => {
                let p: EventsListParams = decode(params)?;
                json!(self.kernel()?.list_task_events_through(
                    &p.task_id,
                    p.after,
                    p.through,
                    p.limit,
                    p.target_bytes
                )?)
            }
            "modelUsage/getSummary" => {
                json!(self.kernel()?.model_usage_summary(&decode(params)?)?)
            }
            "modelUsage/getSummaries" => {
                json!(self.kernel()?.model_usage_summaries(&decode(params)?)?)
            }
            "modelUsage/summarizeRange" => {
                json!(
                    self.kernel()?
                        .summarize_model_usage_range(&decode(params)?)?
                )
            }
            "events/listModelUsage" => {
                let p: EventsListParams = decode(params)?;
                json!(self.kernel()?.list_model_usage_events(&p)?)
            }
            "events/listSessionModelUsage" => {
                let p: EventsListSessionParams = decode(params)?;
                json!(self.kernel()?.list_session_model_usage_events(&p)?)
            }
            "events/listWorkspaceModelUsage" => {
                let p: EventsListWorkspaceParams = decode(params)?;
                json!(self.kernel()?.list_workspace_model_usage_events(&p)?)
            }
            "events/listAll" => {
                let p: EventsListAllParams = decode(params)?;
                json!(self.kernel()?.list_all_events_through(
                    p.after,
                    p.through,
                    p.limit,
                    p.target_bytes
                )?)
            }
            "events/listSession" => {
                let p: EventsListSessionParams = decode(params)?;
                json!(self.kernel()?.list_session_events_through(
                    &p.session_id,
                    p.after,
                    p.through,
                    p.limit,
                    p.target_bytes
                )?)
            }
            "events/listWorkspace" => {
                let p: EventsListWorkspaceParams = decode(params)?;
                json!(self.kernel()?.list_workspace_events_through(
                    &p.workspace_id,
                    p.after,
                    p.through,
                    p.limit,
                    p.target_bytes
                )?)
            }
            "events/listTaskChanges" => {
                let p: TaskChangesParams = decode(params)?;
                json!(self.kernel()?.list_task_changes(&p)?)
            }
            "events/listTaskSummaryChanges" => {
                let p: TaskSummaryChangesParams = decode(params)?;
                json!(self.kernel()?.list_task_summary_changes(p)?)
            }
            "events/listTaskSummaryRevisions" => {
                let p: TaskSummaryChangesParams = decode(params)?;
                json!(self.kernel()?.list_task_summary_revisions(p)?)
            }
            "events/listLatestTaskSummaryRevisions" => {
                let p: TaskSummaryChangesParams = decode(params)?;
                json!(self.kernel()?.list_latest_task_summary_revisions(p)?)
            }
            "events/getWatermark" => {
                if params != json!({}) {
                    return Err(Error::Invalid("events/getWatermark takes no params".into()).into());
                }
                json!(EventWatermark {
                    through_sequence: self.kernel()?.event_watermark()?
                })
            }
            "events/getTaskWatermarks" => {
                let p: TaskWatermarksParams = decode(params)?;
                json!(self.kernel()?.task_event_watermarks(&p)?)
            }
            "events/setScope" => {
                let p: EventsSetScopeParams = decode(params)?;
                let scope = match (p.task_id, p.session_id, p.workspace_id) {
                    (None, None, None) => EventScope::All,
                    (Some(id), None, None) => EventScope::Task(id),
                    (None, Some(id), None) => EventScope::Session(id),
                    (None, None, Some(id)) => EventScope::Workspace(id),
                    _ => {
                        return Err(
                            Error::Invalid("select one event subscription scope".into()).into()
                        );
                    }
                };
                json!(self.replace_event_subscription(
                    scope,
                    p.after,
                    p.watermark_only.unwrap_or(false)
                )?)
            }
            "events/resubscribe" => {
                if params != json!({}) {
                    return Err(Error::Invalid("events/resubscribe takes no params".into()).into());
                }
                if !self.event_stream_gap {
                    return Err(Error::Conflict("event stream is already subscribed".into()).into());
                }
                json!(self.replace_event_subscription(
                    self.event_scope.clone(),
                    None,
                    self.watermark_only
                )?)
            }
            "modelProgress/resubscribe" => {
                if params != json!({}) {
                    return Err(
                        Error::Invalid("modelProgress/resubscribe takes no params".into()).into(),
                    );
                }
                if !self.progress_stream_gap {
                    return Err(Error::Conflict(
                        "model progress stream is already subscribed".into(),
                    )
                    .into());
                }
                let receiver = self.kernel()?.subscribe_model_progress_from_next_step()?;
                self.progress_receiver = Some(receiver);
                self.progress_stream_gap = false;
                json!({"subscribed":true})
            }
            "artifacts/list" => {
                let p: TaskIdParams = decode(params)?;
                json!(self.kernel()?.list_artifacts(&p.task_id)?)
            }
            "artifacts/listSummaries" => {
                let p: ArtifactListParams = decode(params)?;
                json!(
                    self.kernel()?
                        .list_artifact_summaries(&p.task_id, p.after, p.limit)?
                )
            }
            "artifacts/listScopedSummaries" => {
                let p: ArtifactCatalogParams = decode(params)?;
                json!(self.kernel()?.list_artifact_catalog_by_type(
                    &p.scope,
                    p.after,
                    p.limit,
                    p.media_type.as_deref()
                )?)
            }
            "artifacts/listSnapshot" => {
                let p: ArtifactCatalogSnapshotParams = decode(params)?;
                json!(self.kernel()?.list_artifact_snapshot(p)?)
            }
            "artifacts/subscribe" => {
                let p: ArtifactCatalogParams = decode(params)?;
                json!(self.subscribe_artifacts(p)?)
            }
            "artifacts/openDownload" => {
                let p: ArtifactGetParams = decode(params)?;
                let (digest, data) = self
                    .kernel()?
                    .open_artifact_download(&p.task_id, &p.artifact_id)?;
                json!(ArtifactDownloadStart {
                    first_chunk: ArtifactChunk {
                        offset: 0,
                        next_offset: data.len() as u64,
                        content_bytes: digest.content_bytes,
                        data_base64: STANDARD.encode(data),
                    },
                    digest,
                })
            }

            "artifacts/openDownloads" => {
                json!(self.kernel()?.open_artifact_downloads(&decode(params)?)?)
            }
            "artifacts/read" => {
                let p: ArtifactReadParams = decode(params)?;
                let (content_bytes, bytes) = self.kernel()?.read_artifact_chunk(
                    &p.task_id,
                    &p.artifact_id,
                    p.after,
                    p.limit,
                )?;
                json!(ArtifactChunk {
                    offset: p.after,
                    next_offset: p.after + bytes.len() as u64,
                    content_bytes,
                    data_base64: STANDARD.encode(bytes),
                })
            }
            "artifacts/digest" => {
                let p: ArtifactGetParams = decode(params)?;
                json!(self.kernel()?.digest_artifact(&p.task_id, &p.artifact_id)?)
            }
            "artifacts/getDigests" => {
                json!(self.kernel()?.get_artifact_digests(&decode(params)?)?)
            }
            "artifacts/getSummary" => {
                let p: ArtifactGetParams = decode(params)?;
                json!(
                    self.kernel()?
                        .get_artifact_summary(&p.task_id, &p.artifact_id)?
                )
            }
            "artifacts/get" => {
                let p: ArtifactGetParams = decode(params)?;
                json!(self.kernel()?.get_artifact(&p.task_id, &p.artifact_id)?)
            }
            "agents/startSessionList" => {
                let work = self.kernel()?.prepare_agent_session_list(decode(params)?)?;
                let catalog = self.agent_catalog.as_ref().ok_or_else(|| {
                    RpcError::from(Error::Conflict(
                        "agent session listing is not configured".into(),
                    ))
                })?;
                json!(catalog.start(work)?)
            }
            "agents/terminalLogin/start" => {
                let p: AgentTerminalLoginStart = agent_login::decode(params)?;
                let registration = self
                    .agent_terminal_logins
                    .as_ref()
                    .and_then(|service| service.registrations.get(&p.agent_id))
                    .ok_or_else(agent_login::unconfigured)?;
                self.kernel()?.verify_agent_terminal_login(
                    &p.task_id,
                    &p.agent_id,
                    &registration.authority,
                )?;
                let service = self
                    .agent_terminal_logins
                    .as_mut()
                    .ok_or_else(agent_login::unconfigured)?;
                json!(service.start(p)?)
            }
            "agents/terminalLogin/output" => {
                let service = self
                    .agent_terminal_logins
                    .as_mut()
                    .ok_or_else(agent_login::unconfigured)?;
                json!(service.output(agent_login::decode(params)?)?)
            }
            "agents/terminalLogin/input" => {
                let service = self
                    .agent_terminal_logins
                    .as_mut()
                    .ok_or_else(agent_login::unconfigured)?;
                json!(service.input(agent_login::decode(params)?)?)
            }
            "agents/terminalLogin/close" => {
                let service = self
                    .agent_terminal_logins
                    .as_mut()
                    .ok_or_else(agent_login::unconfigured)?;
                service.close(agent_login::decode(params)?)?;
                json!({})
            }
            "agents/getSessionList" => {
                let p: AgentSessionListLookupParams = decode(params)?;
                if uuid::Uuid::parse_str(&p.request_id).is_err() || p.request_id.len() != 36 {
                    return Err(RpcError::from(Error::Invalid(
                        "invalid agent session list request ID".into(),
                    )));
                }
                self.agent_catalog
                    .as_ref()
                    .and_then(|catalog| catalog.get(&p.request_id))
                    .ok_or_else(|| {
                        RpcError::from(Error::NotFound("agent session list request".into()))
                    })?
            }
            "agents/getCapabilities" => {
                let p: AgentIdParams = decode(params)?;
                json!(self.kernel()?.agent_capabilities(&p.agent_id)?)
            }
            "agents/list" => {
                if params != json!({}) {
                    return Err(RpcError {
                        code: -32602,
                        message: "agents/list takes no params".into(),
                    });
                }
                json!(self.kernel()?.agent_specs())
            }
            "tools/list" => {
                if params != json!({}) {
                    return Err(RpcError {
                        code: -32602,
                        message: "tools/list takes no params".into(),
                    });
                }
                json!(
                    self.kernel()?
                        .tool_specs()
                        .into_iter()
                        .map(|s| json!({
                            "name":s.name, "description":s.description, "effect":match s.effect { fliuno_core::ToolEffect::ArtifactWrite => "artifact_write", fliuno_core::ToolEffect::External => "external", fliuno_core::ToolEffect::ProcessExecution => "process_execution", fliuno_core::ToolEffect::TerminalControl => "terminal_control", fliuno_core::ToolEffect::WorkspaceRead => "workspace_read", fliuno_core::ToolEffect::SkillRead => "skill_read", fliuno_core::ToolEffect::WorkspaceWrite => "workspace_write" }, "inputSchema":s.input_schema
                        }))
                        .collect::<Vec<_>>()
                )
            }
            _ => {
                return Err(RpcError {
                    code: -32601,
                    message: "method not found".into(),
                });
            }
        })
    }

    fn terminal_service(&self) -> Result<&fliuno_terminal::Service, RpcError> {
        self.terminals.as_ref().ok_or_else(|| RpcError {
            code: -32004,
            message: "terminal service is not configured".into(),
        })
    }
}

fn terminal_error(message: String) -> RpcError {
    RpcError {
        code: -32602,
        message,
    }
}
fn terminal_runtime() -> Result<tokio::runtime::Runtime, RpcError> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|_| RpcError {
            code: -32603,
            message: "cannot initialize terminal client runtime".into(),
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use fliuno_core::DemoProvider;
    use fliuno_protocol::EventKind;
    use std::{
        sync::{Condvar, mpsc},
        thread,
        time::{Duration, Instant},
    };

    fn server() -> RpcServer {
        RpcServer::new(Kernel::in_memory(Box::new(DemoProvider)).unwrap())
    }

    #[test]
    fn handshake_and_errors() {
        let mut server = server();
        assert_eq!(server.handle_line("{").unwrap()["error"]["code"], -32700);
        assert_eq!(server.handle_line("42").unwrap()["error"]["code"], -32600);
        assert_eq!(
            server
                .handle_line(r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#)
                .unwrap()["error"]["code"],
            -32002
        );
        assert_eq!(server.handle_line(r#"{"jsonrpc":"2.0","id":2,"method":"initialize","params":{"protocolVersion":"9","clientName":"test"}}"#).unwrap()["error"]["code"], -32010);
        let init = r#"{"jsonrpc":"2.0","id":3,"method":"initialize","params":{"protocolVersion":"0.1","clientName":"test"}}"#;
        let initialized = server.handle_line(init).unwrap();
        assert_eq!(initialized["result"]["capabilities"]["liveModels"], false);
        assert_eq!(
            initialized["result"]["capabilities"]["registrationAuthority"],
            true
        );
        assert_eq!(
            initialized["result"]["capabilities"]["modelProgressNotifications"],
            false
        );
        assert_eq!(
            initialized["result"]["capabilities"]["concurrentExecution"],
            true
        );
        assert_eq!(
            initialized["result"]["capabilities"]["maxConcurrentExecutions"],
            scheduler::WORKER_COUNT
        );
        assert_eq!(
            initialized["result"]["capabilities"]["eventNotifications"],
            true
        );
        assert_eq!(
            initialized["result"]["capabilities"]["globalEventReplay"],
            true
        );
        let capabilities = server.handle_line(init).unwrap()["error"]["code"].clone();
        assert_eq!(capabilities, -32009);
        assert_eq!(
            server
                .handle_line(r#"{"jsonrpc":"2.0","id":4,"method":"missing"}"#)
                .unwrap()["error"]["code"],
            -32601
        );
        assert_eq!(server.handle_line(r#"{"jsonrpc":"2.0","id":5,"method":"task/get","params":{"taskId":"x","typo":true}}"#).unwrap()["error"]["code"], -32602);
    }

    #[test]
    fn notifications_batches_and_invalid_ids() {
        let mut server = server();
        assert!(server.handle_line(r#"{"jsonrpc":"2.0","method":"initialize","params":{"protocolVersion":"0.1","clientName":"test"}}"#).is_none());
        let reply = server.handle_line(r#"[{"jsonrpc":"2.0","method":"tools/list"},{"jsonrpc":"2.0","id":"a","method":"tools/list"},null]"#).unwrap();
        assert_eq!(reply.as_array().unwrap().len(), 2);
        assert_eq!(reply[0]["id"], "a");
        assert_eq!(reply[1]["error"]["code"], -32600);
        assert_eq!(server.handle_line("[]").unwrap()["error"]["code"], -32600);
        assert_eq!(
            server
                .handle_line(r#"{"jsonrpc":"2.0","method":"tools/list","id":{}}"#)
                .unwrap()["error"]["code"],
            -32600
        );
    }

    #[test]
    fn rejects_oversize_batches_before_dispatching_any_member() {
        let mut server = server();
        let init = json!({"jsonrpc":"2.0","id":null,"method":"initialize",
            "params":{"protocolVersion":PROTOCOL_VERSION,"clientName":"batch-test"}});
        let read = json!({"jsonrpc":"2.0","id":1,"method":"workspace/list"});
        let mut batch = vec![read; MAX_BATCH_ITEMS + 1];
        batch[0] = init;
        let response = server
            .handle_line(&serde_json::to_string(&batch).unwrap())
            .unwrap();
        assert_eq!(response["error"]["code"], -32600);
        assert!(response["id"].is_null());
        batch.pop();
        let response = server
            .handle_line(&serde_json::to_string(&batch).unwrap())
            .unwrap();
        let responses = response.as_array().unwrap();
        assert_eq!(responses.len(), MAX_BATCH_ITEMS);
        assert_eq!(
            responses[0]["result"]["capabilities"]["maxBatchItems"],
            MAX_BATCH_ITEMS
        );
        assert!(responses[0]["id"].is_null());
        assert!(responses.iter().all(|reply| reply.get("result").is_some()));
    }

    struct BlockingProvider {
        entered: mpsc::Sender<()>,
        release: Arc<(Mutex<bool>, Condvar)>,
    }

    struct StreamingProvider;

    impl fliuno_core::ModelProvider for StreamingProvider {
        fn persistent_identity(&self) -> Option<(String, String)> {
            Some(("test".into(), "streaming-v1".into()))
        }

        fn supports_progress(&self) -> bool {
            true
        }

        fn next(
            &self,
            _task: &Task,
            _tools: &[fliuno_core::ToolSpec],
            _cancellation: &fliuno_core::CancellationToken,
        ) -> std::result::Result<fliuno_core::ModelTurn, String> {
            Ok(fliuno_core::ModelDecision::Complete("final answer".into()).into())
        }

        fn next_with_progress(
            &self,
            _task: &Task,
            _tools: &[fliuno_core::ToolSpec],
            _cancellation: &fliuno_core::CancellationToken,
            progress: &mut dyn FnMut(fliuno_core::ModelStreamEvent),
        ) -> std::result::Result<fliuno_core::ModelTurn, String> {
            progress(fliuno_core::ModelStreamEvent::TextDelta("dr".into()));
            progress(fliuno_core::ModelStreamEvent::TextDelta("aft".into()));
            Ok(fliuno_core::ModelDecision::Complete("final answer".into()).into())
        }
    }

    #[test]
    fn model_progress_resubscribe_excludes_inflight_steps_after_queue_overflow() {
        let kernel = Kernel::in_memory(Box::new(StreamingProvider)).unwrap();
        let mut server = RpcServer::with_provider(kernel, "fixture".into());
        let initialized = rpc(
            &mut server,
            "initialize",
            json!({"protocolVersion":"0.1","clientName":"progress-recovery-test"}),
        );
        assert_eq!(
            initialized["capabilities"]["modelProgressResubscribe"],
            true
        );
        let (receiver, subscription) = server.take_progress_receiver().into_parts();
        let active = server.kernel().unwrap().model_progress_publisher();
        let stale = ModelProgress::TextDelta {
            task_id: "task-active".into(),
            step: 1,
            text: "suffix".into(),
        };
        for _ in 0..65 {
            active.publish(stale.clone());
        }
        assert_eq!(receiver.iter().count(), 64);
        drop(subscription);
        server.mark_progress_stream_gap();
        let resubscribe = r#"{"jsonrpc":"2.0","id":10,"method":"modelProgress/resubscribe"}"#;
        assert_eq!(
            server.handle_line(resubscribe).unwrap()["result"],
            json!({"subscribed":true})
        );
        assert_eq!(
            server.handle_line(resubscribe).unwrap()["error"]["code"],
            -32009
        );
        let replacement = server.take_pending_progress_receiver().unwrap();
        active.publish(stale);
        active.publish(ModelProgress::Reset {
            task_id: "task-active".into(),
            step: 1,
        });
        assert!(matches!(
            replacement.try_recv(),
            Err(crossbeam_channel::TryRecvError::Empty)
        ));

        let fresh = server.kernel().unwrap().model_progress_publisher();
        let preview = ModelProgress::TextDelta {
            task_id: "task-active".into(),
            step: 2,
            text: "new preview".into(),
        };
        fresh.publish(preview.clone());
        assert_eq!(replacement.try_recv().unwrap(), preview);
    }

    #[test]
    fn model_progress_is_volatile_and_tied_to_the_task_step() {
        let mut kernel = Kernel::in_memory(Box::new(StreamingProvider)).unwrap();
        let workspace = kernel
            .open_workspace(std::env::current_dir().unwrap().to_str().unwrap())
            .unwrap();
        let session = kernel.create_session(&workspace.id).unwrap();
        let mut server = RpcServer::with_provider(kernel, "fixture".into());
        let initialized = rpc(
            &mut server,
            "initialize",
            json!({"protocolVersion":"0.1","clientName":"progress-test"}),
        );
        assert_eq!(
            initialized["capabilities"]["modelProgressNotifications"],
            true
        );
        let (progress, _subscription) = server.take_progress_receiver().into_parts();
        let task = rpc(
            &mut server,
            "task/start",
            json!({"sessionId":session.id,"input":"stream"}),
        );
        let deltas = [
            progress.recv_timeout(Duration::from_secs(2)).unwrap(),
            progress.recv_timeout(Duration::from_secs(2)).unwrap(),
        ];
        let text = deltas
            .iter()
            .map(|delta| match delta {
                ModelProgress::TextDelta {
                    task_id: event_task,
                    step,
                    text,
                } => {
                    assert_eq!(event_task, task["id"].as_str().unwrap());
                    assert_eq!(*step, 1);
                    text.as_str()
                }
                ModelProgress::Reset { .. } => panic!("unexpected progress reset"),
            })
            .collect::<String>();
        assert_eq!(text, "draft");
        let deadline = Instant::now() + Duration::from_secs(2);
        let finished = loop {
            let current = rpc(&mut server, "task/get", json!({"taskId":task["id"]}));
            if current["status"] == "completed" {
                break current;
            }
            assert!(Instant::now() < deadline, "streaming task did not finish");
            thread::sleep(Duration::from_millis(5));
        };
        assert_eq!(finished["output"], "final answer");
        let events = server
            .kernel()
            .unwrap()
            .list_events(task["id"].as_str().unwrap(), 0, 100)
            .unwrap()
            .events;
        assert_eq!(events.len(), 3);
        assert!(
            events
                .iter()
                .any(|event| matches!(event.data, EventKind::ModelStarted { step: 1 }))
        );
        assert!(
            events
                .iter()
                .any(|event| matches!(event.data, EventKind::TaskCompleted { .. }))
        );
    }

    impl fliuno_core::ModelProvider for BlockingProvider {
        fn persistent_identity(&self) -> Option<(String, String)> {
            Some(("fixture".into(), "blocking-v1".into()))
        }

        fn next(
            &self,
            _task: &Task,
            _tools: &[fliuno_core::ToolSpec],
            _cancellation: &fliuno_core::CancellationToken,
        ) -> std::result::Result<fliuno_core::ModelTurn, String> {
            self.entered.send(()).map_err(|_| "test receiver closed")?;
            let (lock, changed) = &*self.release;
            let released = lock.lock().map_err(|_| "test lock poisoned")?;
            let (released, _) = changed
                .wait_timeout_while(released, Duration::from_secs(5), |released| !*released)
                .map_err(|_| "test lock poisoned")?;
            if !*released {
                return Err("test fixture was not released".into());
            }
            Ok(fliuno_core::ModelDecision::Complete("late response".into()).into())
        }
    }

    #[test]
    fn running_model_request_does_not_block_query_or_cancel_and_late_result_is_ignored() {
        let (entered_tx, entered_rx) = mpsc::channel();
        let release = Arc::new((Mutex::new(false), Condvar::new()));
        let mut kernel = Kernel::in_memory(Box::new(BlockingProvider {
            entered: entered_tx,
            release: Arc::clone(&release),
        }))
        .unwrap();
        let workspace = kernel
            .open_workspace(std::env::current_dir().unwrap().to_str().unwrap())
            .unwrap();
        let session = kernel.create_session(&workspace.id).unwrap();
        let mut server = RpcServer::new(kernel);
        server.handle_line(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"0.1","clientName":"scheduler-test"}}"#);
        let started = server
            .handle_line(&format!(
                r#"{{"jsonrpc":"2.0","id":2,"method":"task/start","params":{{"sessionId":"{}","input":"wait"}}}}"#,
                session.id
            ))
            .unwrap();
        let task_id = started["result"]["id"].as_str().unwrap();
        assert_eq!(started["result"]["status"], "running");
        entered_rx.recv_timeout(Duration::from_secs(2)).unwrap();

        let queried = server
            .handle_line(&format!(
                r#"{{"jsonrpc":"2.0","id":3,"method":"task/get","params":{{"taskId":"{task_id}"}}}}"#
            ))
            .unwrap();
        assert_eq!(queried["result"]["status"], "running");
        let cancelled = server
            .handle_line(&format!(
                r#"{{"jsonrpc":"2.0","id":4,"method":"task/cancel","params":{{"taskId":"{task_id}"}}}}"#
            ))
            .unwrap();
        assert_eq!(cancelled["result"]["status"], "cancelled");

        let (lock, changed) = &*release;
        *lock.lock().unwrap() = true;
        changed.notify_all();
        let deadline = Instant::now() + Duration::from_secs(2);
        while !server.scheduler.is_idle() {
            assert!(Instant::now() < deadline, "scheduler did not finish");
            thread::sleep(Duration::from_millis(5));
        }
        let final_task = server
            .handle_line(&format!(
                r#"{{"jsonrpc":"2.0","id":5,"method":"task/get","params":{{"taskId":"{task_id}"}}}}"#
            ))
            .unwrap();
        assert_eq!(final_task["result"]["status"], "cancelled");
        assert_eq!(final_task["result"]["output"], Value::Null);
    }

    struct ToolThenComplete;

    impl fliuno_core::ModelProvider for ToolThenComplete {
        fn next(
            &self,
            task: &Task,
            _tools: &[fliuno_core::ToolSpec],
            _cancellation: &fliuno_core::CancellationToken,
        ) -> std::result::Result<fliuno_core::ModelTurn, String> {
            if task
                .messages
                .iter()
                .any(|message| matches!(message, Message::ToolResult { .. }))
            {
                Ok(fliuno_core::ModelDecision::Complete("tool result accepted".into()).into())
            } else {
                Ok(fliuno_core::ModelDecision::Call(fliuno_protocol::ToolCall {
                    id: "slow-call".into(),
                    name: "slow_tool".into(),
                    arguments: json!({}),
                })
                .into())
            }
        }
    }

    struct BlockingTool {
        entered: mpsc::Sender<()>,
        release: Arc<(Mutex<bool>, Condvar)>,
        cancellation_seen: Arc<std::sync::atomic::AtomicBool>,
    }

    impl fliuno_core::Tool for BlockingTool {
        fn spec(&self) -> fliuno_core::ToolSpec {
            fliuno_core::ToolSpec {
                name: "slow_tool".into(),
                description: "Blocking test tool".into(),
                effect: fliuno_core::ToolEffect::External,
                input_schema: json!({"type":"object","additionalProperties":false}),
            }
        }

        fn validate(&self, arguments: &Value) -> std::result::Result<(), String> {
            if arguments == &json!({}) {
                Ok(())
            } else {
                Err("expected empty arguments".into())
            }
        }

        fn execute(
            &self,
            _context: &fliuno_core::ToolContext<'_>,
            _call: &fliuno_protocol::ToolCall,
            cancellation: &fliuno_core::CancellationToken,
        ) -> std::result::Result<fliuno_core::ToolOutput, fliuno_core::ToolError> {
            self.entered.send(()).map_err(|_| "test receiver closed")?;
            let (lock, changed) = &*self.release;
            let released = lock.lock().map_err(|_| "test lock poisoned")?;
            let (released, _) = changed
                .wait_timeout_while(released, Duration::from_secs(5), |released| !*released)
                .map_err(|_| "test lock poisoned")?;
            self.cancellation_seen.store(
                cancellation.is_cancelled(),
                std::sync::atomic::Ordering::Release,
            );
            if !*released {
                return Err("test fixture was not released".into());
            }
            Ok(fliuno_core::ToolOutput {
                output: json!({"ok":true}),
                artifacts: vec![],
            })
        }
    }

    #[test]
    fn slow_tool_does_not_block_rpc_and_cancel_discards_its_late_result() {
        let (entered_tx, entered_rx) = mpsc::channel();
        let release = Arc::new((Mutex::new(false), Condvar::new()));
        let cancellation_seen = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut kernel = Kernel::in_memory(Box::new(ToolThenComplete)).unwrap();
        kernel
            .register_tool(Box::new(BlockingTool {
                entered: entered_tx,
                release: Arc::clone(&release),
                cancellation_seen: Arc::clone(&cancellation_seen),
            }))
            .unwrap();
        let workspace = kernel
            .open_workspace(std::env::current_dir().unwrap().to_str().unwrap())
            .unwrap();
        let session = kernel.create_session(&workspace.id).unwrap();
        let mut server = RpcServer::new(kernel);
        rpc(
            &mut server,
            "initialize",
            json!({"protocolVersion":"0.1","clientName":"slow-tool-test"}),
        );
        let task = rpc(
            &mut server,
            "task/start",
            json!({"sessionId":session.id,"input":"run slow tool"}),
        );
        let deadline = Instant::now() + Duration::from_secs(2);
        let waiting = loop {
            let task = rpc(&mut server, "task/get", json!({"taskId":task["id"]}));
            if task["status"] == "waiting_approval" {
                break task;
            }
            assert!(Instant::now() < deadline, "approval was not requested");
            thread::sleep(Duration::from_millis(5));
        };

        let before = Instant::now();
        let running = rpc(
            &mut server,
            "approval/respond",
            json!({
                "taskId":waiting["id"],
                "approvalId":waiting["pendingApproval"]["id"],
                "decision":"allow_once"
            }),
        );
        assert!(before.elapsed() < Duration::from_millis(250));
        assert_eq!(running["status"], "running");
        entered_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(
            rpc(&mut server, "task/get", json!({"taskId":waiting["id"]}))["status"],
            "running"
        );
        assert_eq!(
            rpc(&mut server, "task/cancel", json!({"taskId":waiting["id"]}))["status"],
            "cancelled"
        );

        let (lock, changed) = &*release;
        *lock.lock().unwrap() = true;
        changed.notify_all();
        let deadline = Instant::now() + Duration::from_secs(2);
        while !server.scheduler.is_idle() {
            assert!(Instant::now() < deadline, "tool worker did not finish");
            thread::sleep(Duration::from_millis(5));
        }
        let cancelled = rpc(&mut server, "task/get", json!({"taskId":waiting["id"]}));
        assert_eq!(cancelled["status"], "cancelled");
        assert_eq!(cancelled["output"], Value::Null);
        assert!(cancellation_seen.load(std::sync::atomic::Ordering::Acquire));
        let events = server
            .kernel()
            .unwrap()
            .list_events(waiting["id"].as_str().unwrap(), 0, 100)
            .unwrap();
        assert!(!events.events.iter().any(|event| matches!(
            event.data,
            EventKind::ToolCompleted { .. } | EventKind::TaskCompleted { .. }
        )));
    }

    fn rpc(server: &mut RpcServer, method: &str, params: Value) -> Value {
        let response = server
            .handle_line(
                &json!({
                    "jsonrpc":"2.0", "id":1, "method":method, "params":params
                })
                .to_string(),
            )
            .unwrap();
        assert!(response.get("error").is_none(), "{response}");
        response["result"].clone()
    }

    #[test]
    fn durable_run_rpc_schedules_dependencies_to_completion() {
        let mut kernel = Kernel::in_memory(Box::new(StreamingProvider)).unwrap();
        let workspace = kernel
            .open_workspace(std::env::current_dir().unwrap().to_str().unwrap())
            .unwrap();
        let session = kernel.create_session(&workspace.id).unwrap();
        let mut server = RpcServer::with_provider(kernel, "fixture".into());
        let initialized = rpc(
            &mut server,
            "initialize",
            json!({"protocolVersion":"0.1","clientName":"run-test"}),
        );
        assert_eq!(initialized["capabilities"]["durableTaskRuns"], true);
        let run = rpc(
            &mut server,
            "run/start",
            json!({
                "sessionId":session.id,
                "clientRequestId":"run-process-test",
                "maxConcurrency":1,
                "nodes":[
                    {"nodeId":"first","input":"first"},
                    {"nodeId":"second","input":"second","dependsOn":["first"]}
                ]
            }),
        );
        let run_id = run["id"].as_str().unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        let finished = loop {
            let run = rpc(&mut server, "run/get", json!({"runId":run_id}));
            if run["status"] == "completed" {
                break run;
            }
            assert!(std::time::Instant::now() < deadline, "run remained {run}");
            std::thread::sleep(std::time::Duration::from_millis(5));
        };
        assert!(
            finished["nodes"]
                .as_array()
                .unwrap()
                .iter()
                .all(|node| { node["status"] == "completed" && node["taskId"].is_string() })
        );
        let events = rpc(
            &mut server,
            "run/events",
            json!({"runId":run_id,"after":0,"limit":64}),
        );
        let started = events["events"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|event| event["data"]["type"] == "node_started")
            .map(|event| event["data"]["nodeId"].as_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(started, ["first", "second"]);
    }

    #[test]
    fn run_deadline_discards_a_late_model_result() {
        let (entered_tx, entered_rx) = mpsc::channel();
        let release = Arc::new((Mutex::new(false), Condvar::new()));
        let mut kernel = Kernel::in_memory(Box::new(BlockingProvider {
            entered: entered_tx,
            release: Arc::clone(&release),
        }))
        .unwrap();
        let workspace = kernel
            .open_workspace(std::env::current_dir().unwrap().to_str().unwrap())
            .unwrap();
        let session = kernel.create_session(&workspace.id).unwrap();
        let mut server = RpcServer::new(kernel);
        rpc(
            &mut server,
            "initialize",
            json!({"protocolVersion":"0.1","clientName":"deadline"}),
        );
        let run = rpc(
            &mut server,
            "run/start",
            json!({
                "sessionId":session.id,"clientRequestId":"late-model","timeoutMs":1000,
                "nodes":[{"nodeId":"model","input":"wait"}]
            }),
        );
        let task_id = run["nodes"][0]["taskId"].as_str().unwrap();
        entered_rx.recv_timeout(Duration::from_secs(2)).unwrap();

        let wait = Instant::now() + Duration::from_secs(3);
        let cancelled = loop {
            let task = rpc(&mut server, "task/get", json!({"taskId":task_id}));
            if task["status"] == "cancelled" {
                break task;
            }
            assert!(Instant::now() < wait, "deadline did not settle: {task}");
            thread::sleep(Duration::from_millis(5));
        };
        assert_eq!(cancelled["modelOutcomeUnknown"], true);
        let (lock, changed) = &*release;
        *lock.lock().unwrap() = true;
        changed.notify_all();
        let wait = Instant::now() + Duration::from_secs(2);
        while !server.scheduler.is_idle() {
            assert!(
                Instant::now() < wait,
                "late provider result was not drained"
            );
            thread::sleep(Duration::from_millis(5));
        }
        let task = rpc(&mut server, "task/get", json!({"taskId":task_id}));
        assert_eq!(task["status"], "cancelled");
        assert_eq!(task["modelSteps"], 1);
        assert_eq!(task["output"], Value::Null);
        assert_eq!(
            rpc(&mut server, "run/get", json!({"runId":run["id"]}))["status"],
            "timed_out"
        );
    }

    struct InputProvider;

    impl fliuno_core::ModelProvider for InputProvider {
        fn next(
            &self,
            task: &Task,
            _: &[fliuno_core::ToolSpec],
            _: &fliuno_core::CancellationToken,
        ) -> std::result::Result<fliuno_core::ModelTurn, String> {
            if task
                .messages
                .iter()
                .any(|message| matches!(message, Message::ToolResult { .. }))
            {
                Ok(fliuno_core::ModelDecision::Complete("accepted".into()).into())
            } else {
                Ok(fliuno_core::ModelDecision::Call(fliuno_protocol::ToolCall {
                    id: "input-call".into(),
                    name: "input_tool".into(),
                    arguments: json!({"target":"service"}),
                })
                .into())
            }
        }
    }

    struct InputTool(Arc<std::sync::atomic::AtomicUsize>);

    impl fliuno_core::Tool for InputTool {
        fn spec(&self) -> fliuno_core::ToolSpec {
            fliuno_core::ToolSpec {
                name: "input_tool".into(),
                description: "Test tool input".into(),
                effect: fliuno_core::ToolEffect::External,
                input_schema: json!({"type":"object","required":["target"],"properties":{"target":{"type":"string"}},"additionalProperties":false}),
            }
        }

        fn validate(&self, arguments: &Value) -> std::result::Result<(), String> {
            (arguments == &json!({"target":"service"}))
                .then_some(())
                .ok_or_else(|| "invalid fixture arguments".into())
        }

        fn execute(
            &self,
            _: &fliuno_core::ToolContext<'_>,
            _: &fliuno_protocol::ToolCall,
            _: &fliuno_core::CancellationToken,
        ) -> std::result::Result<fliuno_core::ToolOutput, fliuno_core::ToolError> {
            Err(fliuno_core::ToolError::InputRequired {
                requests: json!({"confirmation":{"message":"Confirm"}}),
                request_state: Some("private-state".into()),
            })
        }

        fn validate_input_request(
            &self,
            request: &fliuno_protocol::ToolInputRequest,
            state: Option<&str>,
        ) -> std::result::Result<(), String> {
            (request.requests == json!({"confirmation":{"message":"Confirm"}})
                && state == Some("private-state"))
            .then_some(())
            .ok_or_else(|| "invalid fixture request".into())
        }

        fn validate_input(
            &self,
            _: &fliuno_protocol::ToolInputRequest,
            responses: &Value,
        ) -> std::result::Result<(), String> {
            (responses == &json!({"confirmation":{"accepted":true}}))
                .then_some(())
                .ok_or_else(|| "invalid fixture response".into())
        }

        fn resume(
            &self,
            _: &fliuno_core::ToolContext<'_>,
            _: &fliuno_protocol::ToolCall,
            state: Option<&str>,
            _: &Value,
            _: &fliuno_core::CancellationToken,
        ) -> std::result::Result<fliuno_core::ToolOutput, fliuno_core::ToolError> {
            if state != Some("private-state") {
                return Err(fliuno_core::ToolError::Failed(
                    "continuation state changed".into(),
                ));
            }
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(fliuno_core::ToolOutput {
                output: json!({"accepted":true}),
                artifacts: vec![],
            })
        }
    }

    #[test]
    fn tool_input_rpc_schedules_one_resumable_approved_call() {
        let resumed = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut kernel = Kernel::in_memory(Box::new(InputProvider)).unwrap();
        kernel
            .register_tool(Box::new(InputTool(Arc::clone(&resumed))))
            .unwrap();
        let workspace = kernel
            .open_workspace(std::env::current_dir().unwrap().to_str().unwrap())
            .unwrap();
        let session = kernel.create_session(&workspace.id).unwrap();
        let mut server = RpcServer::new(kernel);
        let init = rpc(
            &mut server,
            "initialize",
            json!({"protocolVersion":"0.1","clientName":"tool-input-test"}),
        );
        assert_eq!(init["capabilities"]["taskRespondToolInput"], true);
        let started = rpc(
            &mut server,
            "task/start",
            json!({"sessionId":session.id,"input":"continue"}),
        );
        let task_id = started["id"].as_str().unwrap().to_owned();
        let wait = |server: &mut RpcServer, expected: &str| {
            let deadline = Instant::now() + Duration::from_secs(2);
            loop {
                let task = rpc(server, "task/get", json!({"taskId":task_id}));
                if task["status"] == expected {
                    return task;
                }
                assert!(
                    Instant::now() < deadline,
                    "task never entered {expected}: {task}"
                );
                thread::sleep(Duration::from_millis(5));
            }
        };
        let waiting_approval = wait(&mut server, "waiting_approval");
        rpc(
            &mut server,
            "approval/respond",
            json!({"taskId":task_id,"approvalId":waiting_approval["pendingApproval"]["id"],"decision":"allow_once"}),
        );
        let waiting_input = wait(&mut server, "waiting_input");
        assert_eq!(waiting_input["pendingToolInput"]["callId"], "input-call");
        assert_eq!(
            waiting_input["inFlightTool"]["arguments"],
            json!({"target":"service"})
        );
        assert!(!waiting_input.to_string().contains("private-state"));

        let response = json!({
            "taskId":task_id,"callId":"input-call","requestId":"input-response-1",
            "responses":{"confirmation":{"accepted":true}}
        });
        rpc(&mut server, "task/respondToolInput", response.clone());
        let completed = wait(&mut server, "completed");
        assert_eq!(completed["output"], "accepted");
        assert_eq!(resumed.load(std::sync::atomic::Ordering::SeqCst), 1);
        let duplicate = rpc(&mut server, "task/respondToolInput", response);
        assert_eq!(duplicate["status"], "completed");
        assert_eq!(resumed.load(std::sync::atomic::Ordering::SeqCst), 1);
        let events = server
            .kernel()
            .unwrap()
            .list_events(&task_id, 0, 100)
            .unwrap();
        assert!(!events.events.iter().any(|event| {
            serde_json::to_string(&event.data)
                .unwrap()
                .contains("private-state")
        }));
    }

    #[test]
    fn queue_is_bounded_and_cancelled_queued_tasks_never_call_the_provider() {
        let (entered_tx, entered_rx) = mpsc::channel();
        let release = Arc::new((Mutex::new(false), Condvar::new()));
        let mut kernel = Kernel::in_memory(Box::new(BlockingProvider {
            entered: entered_tx,
            release: Arc::clone(&release),
        }))
        .unwrap();
        let workspace = kernel
            .open_workspace(std::env::current_dir().unwrap().to_str().unwrap())
            .unwrap();
        let session = kernel.create_session(&workspace.id).unwrap();
        let mut server = RpcServer::new(kernel);
        rpc(
            &mut server,
            "initialize",
            json!({"protocolVersion":"0.1","clientName":"queue-test"}),
        );
        rpc(
            &mut server,
            "task/start",
            json!({"sessionId":session.id,"input":"hold worker"}),
        );
        entered_rx.recv_timeout(Duration::from_secs(2)).unwrap();

        let mut queued = Vec::new();
        let accepted = scheduler::QUEUE_CAPACITY + scheduler::WORKER_COUNT - 1;
        for index in 0..accepted {
            let task = rpc(
                &mut server,
                "task/start",
                json!({
                    "sessionId":session.id,"input":"queued", "clientRequestId":format!("queue-{index}")
                }),
            );
            assert_eq!(task["status"], "running");
            queued.push(task);
        }
        let full = rpc(
            &mut server,
            "task/start",
            json!({
                "sessionId":session.id,"input":"overflow", "clientRequestId":"overflow"
            }),
        );
        assert_eq!(full["status"], "failed");
        assert_eq!(full["modelSteps"], 0);
        assert_eq!(full["output"], "task scheduler queue is full");
        let retry = rpc(
            &mut server,
            "task/start",
            json!({
                "sessionId":session.id,"input":"overflow", "clientRequestId":"overflow"
            }),
        );
        assert_eq!(retry["id"], full["id"]);
        let cancelled_task = queued.last().unwrap();
        rpc(
            &mut server,
            "task/cancel",
            json!({"taskId":cancelled_task["id"]}),
        );
        let (lock, changed) = &*release;
        *lock.lock().unwrap() = true;
        changed.notify_all();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !server.scheduler.is_idle() {
            assert!(
                Instant::now() < deadline,
                "scheduler did not drain the queue"
            );
            thread::sleep(Duration::from_millis(5));
        }
        let cancelled = rpc(
            &mut server,
            "task/get",
            json!({"taskId":cancelled_task["id"]}),
        );
        assert_eq!(cancelled["status"], "cancelled");
        assert_eq!(cancelled["modelSteps"], 0);
        assert_eq!(
            entered_rx.try_iter().count(),
            scheduler::QUEUE_CAPACITY + scheduler::WORKER_COUNT - 2
        );
        for task in queued.iter().take(accepted - 1) {
            assert_eq!(
                rpc(&mut server, "task/get", json!({"taskId":task["id"]}))["status"],
                "completed"
            );
        }
    }
}
