//! JSON-RPC 2.0 dispatch. Transport and domain errors never enter model messages.
mod scheduler;
mod terminal_subscriptions;

use base64::{Engine as _, engine::general_purpose::STANDARD};
use fliuno_core::{ApprovalOutcome, Error, EventReceiver, Kernel, ProgressReceiver};
use fliuno_protocol::*;
use scheduler::TaskScheduler;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex, MutexGuard};

const MAX_BATCH_ITEMS: usize = 128;

pub struct RpcServer {
    kernel: Arc<Mutex<Kernel>>,
    scheduler: TaskScheduler,
    event_receiver: Option<EventReceiver>,
    event_stream_gap: bool,
    progress_receiver: Option<ProgressReceiver>,
    progress_stream_gap: bool,
    initialized: bool,
    provider_name: String,
    terminals: Option<fliuno_terminal::Service>,
    terminal_subscriptions: terminal_subscriptions::Subscriptions,
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
        kernel: Kernel,
        provider_name: String,
        terminals: Option<fliuno_terminal::Service>,
    ) -> Self {
        let event_receiver = kernel
            .subscribe_events()
            .expect("cannot subscribe app-server to kernel events");
        let progress_receiver = kernel
            .subscribe_model_progress()
            .expect("cannot subscribe app-server to model progress");
        let kernel = Arc::new(Mutex::new(kernel));
        let scheduler = TaskScheduler::new(Arc::clone(&kernel));
        Self {
            kernel,
            scheduler,
            event_receiver: Some(event_receiver),
            event_stream_gap: false,
            progress_receiver: Some(progress_receiver),
            progress_stream_gap: false,
            initialized: false,
            provider_name,
            terminals,
            terminal_subscriptions: terminal_subscriptions::Subscriptions::new(),
        }
    }

    pub fn take_event_receiver(&mut self) -> EventReceiver {
        self.take_pending_event_receiver()
            .expect("app-server event receiver was already taken")
    }

    pub fn take_pending_event_receiver(&mut self) -> Option<EventReceiver> {
        self.event_receiver.take()
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
            capabilities["eventBySequence"] = json!(true);
            capabilities["contextConversationDetail"] = json!(true);
            capabilities["contextAtSequence"] = json!(true);
            capabilities["taskSummaryById"] = json!(true);
            capabilities["taskSummaryStatusFilter"] = json!(true);
            capabilities["pendingApprovalInbox"] = json!(true);
            capabilities["pendingApprovalDetails"] = json!(true);
            capabilities["sessionArchiving"] = json!(true);
            capabilities["artifactByteReads"] = json!(true);
            capabilities["artifactSummaryById"] = json!(true);
            capabilities["maxArtifactReadBytes"] = json!(MAX_ARTIFACT_READ_BYTES);
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
            "workspace/open" => {
                let p: WorkspaceOpenParams = decode(params)?;
                json!(self.kernel()?.open_workspace(&p.root)?)
            }
            "workspace/list" => {
                let p: ListParams = decode(params)?;
                json!(self.kernel()?.list_workspaces(p.after, p.limit)?)
            }
            "session/list" => {
                let p: SessionListParams = decode(params)?;
                json!(self.kernel()?.list_sessions_filtered(
                    &p.workspace_id,
                    p.after,
                    p.limit,
                    p.archived
                )?)
            }
            "task/list" => {
                let p: TaskListParams = decode(params)?;
                json!(self.kernel()?.list_tasks(&p.session_id, p.after, p.limit)?)
            }
            "task/listSummaries" => {
                let p: TaskSummaryListParams = decode(params)?;
                json!(self.kernel()?.list_task_summaries_filtered(
                    &p.session_id,
                    p.after,
                    p.limit,
                    p.status
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
            "session/context" => {
                let p: SessionContextParams = decode(params)?;
                json!(self.kernel()?.session_context_through(
                    &p.session_id,
                    p.options,
                    p.through_sequence,
                )?)
            }
            "task/start" => {
                let p: TaskStartParams = decode(params)?;
                let (task, created) = self.kernel()?.create_task_with_context(
                    &p.session_id,
                    &p.input,
                    p.client_request_id.as_deref(),
                    p.backend,
                    p.context,
                )?;
                json!(if created {
                    self.schedule_or_fail(task)?
                } else {
                    task
                })
            }
            "task/getSummary" => {
                let p: TaskIdParams = decode(params)?;
                json!(self.kernel()?.get_task_summary(&p.task_id)?)
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
            "task/checkRemote" => {
                let p: TaskIdParams = decode(params)?;
                let check = self.kernel()?.prepare_remote_task_check(&p.task_id)?;
                let accepted = RemoteTaskCheckAccepted {
                    check_id: check.check_id().to_owned(),
                };
                self.scheduler
                    .schedule_remote_check(check)
                    .map_err(|message| Error::Conflict(message.into()))?;
                json!(accepted)
            }
            "task/cancel" => {
                let p: TaskIdParams = decode(params)?;
                self.scheduler
                    .cancel(&p.task_id)
                    .map_err(|message| RpcError {
                        code: -32603,
                        message: message.into(),
                    })?;
                json!(self.kernel()?.cancel_task(&p.task_id)?)
            }
            "task/resume" => {
                let p: TaskResumeParams = decode(params)?;
                let task = self
                    .kernel()?
                    .prepare_resume_task(&p.task_id, p.retry_uncertain_model_step)?;
                json!(self.schedule_or_fail(task)?)
            }
            "approval/listPending" => {
                let p: ApprovalListParams = decode(params)?;
                json!(self.kernel()?.list_pending_approvals(p)?)
            }
            "approval/get" => {
                let p: ApprovalGetParams = decode(params)?;
                json!(
                    self.kernel()?
                        .get_pending_approval(&p.task_id, &p.approval_id)?
                )
            }
            "approval/respond" => {
                let p: ApprovalRespondParams = decode(params)?;
                match self.kernel()?.prepare_approval_response(
                    &p.task_id,
                    &p.approval_id,
                    p.decision,
                )? {
                    ApprovalOutcome::Finished(task) => json!(task),
                    ApprovalOutcome::Prepared { task, execution } => {
                        match self.scheduler.schedule_tool(execution) {
                            Ok(()) => json!(task),
                            Err(message) => {
                                json!(self.kernel()?.fail_task(&task.id, message.into())?)
                            }
                        }
                    }
                }
            }
            "events/get" => {
                let p: EventGetParams = decode(params)?;
                json!(self.kernel()?.get_event(p.sequence)?)
            }
            "events/list" => {
                let p: EventsListParams = decode(params)?;
                json!(self.kernel()?.list_events_with_target(
                    &p.task_id,
                    p.after,
                    p.limit,
                    p.target_bytes
                )?)
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
            "events/resubscribe" => {
                if params != json!({}) {
                    return Err(Error::Invalid("events/resubscribe takes no params".into()).into());
                }
                if !self.event_stream_gap {
                    return Err(Error::Conflict("event stream is already subscribed".into()).into());
                }
                let (through_sequence, receiver) = {
                    // Committing workers hold this lock. The watermark and
                    // subscription therefore cover adjacent event ranges.
                    let kernel = self.kernel()?;
                    (kernel.event_watermark()?, kernel.subscribe_events()?)
                };
                self.event_receiver = Some(receiver);
                self.event_stream_gap = false;
                json!(EventSubscription { through_sequence })
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
