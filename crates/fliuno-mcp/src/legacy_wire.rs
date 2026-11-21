//! Preserve Tasks fields that the current SDK's typed model does not retain.
use crate::tasks::watch;
use rmcp::{
    RoleClient,
    model::{
        CancelledNotification, ClientRequest, CustomRequest, CustomResult, Extensions,
        GetExtensions, GetMeta, JsonRpcMessage, NotificationMetaObject, RequestId,
        ServerNotification, ServerResult,
    },
    service::{RxJsonRpcMessage, ServiceRole, TxJsonRpcMessage},
    transport::Transport,
};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, HashMap},
    sync::{Arc, Mutex},
};
use tokio::sync::oneshot;

#[derive(Clone, Copy, Debug)]
pub(super) struct WireClient;

// This role is used only by the line codec. The running service still uses
// RoleClient for lifecycle, cancellation, and response correlation.
impl ServiceRole for WireClient {
    type Req = <RoleClient as ServiceRole>::Req;
    type Resp = <RoleClient as ServiceRole>::Resp;
    type Not = <RoleClient as ServiceRole>::Not;
    type PeerReq = <RoleClient as ServiceRole>::PeerReq;
    type PeerResp = Value;
    type PeerNot = WireNotification;
    type InitializeError = <RoleClient as ServiceRole>::InitializeError;
    type Info = <RoleClient as ServiceRole>::Info;
    type PeerInfo = <RoleClient as ServiceRole>::PeerInfo;
    const IS_CLIENT: bool = true;
}

#[derive(Clone, Debug, Serialize)]
#[serde(transparent)]
pub(super) struct WireNotification {
    raw: Value,
    #[serde(skip)]
    typed: ServerNotification,
}

impl<'de> Deserialize<'de> for WireNotification {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = Value::deserialize(deserializer)?;
        let typed = ServerNotification::deserialize(&raw).map_err(serde::de::Error::custom)?;
        Ok(Self { raw, typed })
    }
}

impl From<CancelledNotification> for WireNotification {
    fn from(notice: CancelledNotification) -> Self {
        let typed = ServerNotification::from(notice);
        let raw = serde_json::to_value(&typed).expect("SDK cancellation notification serializes");
        Self { raw, typed }
    }
}

impl TryFrom<WireNotification> for CancelledNotification {
    type Error = WireNotification;

    fn try_from(notice: WireNotification) -> Result<Self, Self::Error> {
        notice.typed.try_into().map_err(|typed| WireNotification {
            raw: notice.raw,
            typed,
        })
    }
}

impl GetMeta for WireNotification {
    type Metadata = NotificationMetaObject;

    fn get_meta_mut(&mut self) -> &mut Self::Metadata {
        self.typed.get_meta_mut()
    }

    fn get_meta(&self) -> &Self::Metadata {
        self.typed.get_meta()
    }
}

impl GetExtensions for WireNotification {
    fn extensions(&self) -> &Extensions {
        self.typed.extensions()
    }

    fn extensions_mut(&mut self) -> &mut Extensions {
        self.typed.extensions_mut()
    }
}

#[derive(Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(super) enum TaskSupport {
    Forbidden,
    Optional,
    Required,
}

#[derive(Clone, Default)]
pub(super) struct Registration {
    pub call: bool,
    pub cancel: bool,
    pub tools: BTreeMap<String, TaskSupport>,
}

#[derive(Clone, Copy)]
enum Capture {
    Initialize,
    Tools,
    RawTask,
}

#[derive(Default)]
struct State {
    pending: HashMap<RequestId, Capture>,
    http_replies: HashMap<RequestId, Value>,
    registration: Option<Registration>,
    invalid: bool,
    watch: Option<WatchTarget>,
    modern_watch: Option<watch::Subscriber>,
}

#[derive(Clone, Default)]
pub(super) struct Negotiation(Arc<Mutex<State>>);

struct WatchTarget {
    task_id: String,
    created_at: String,
    signal: Option<oneshot::Sender<()>>,
}

pub(super) struct StatusWatch {
    negotiation: Negotiation,
    pub signal: oneshot::Receiver<()>,
}

impl Drop for StatusWatch {
    fn drop(&mut self) {
        self.negotiation
            .0
            .lock()
            .expect("legacy MCP negotiation lock poisoned")
            .watch = None;
    }
}

pub(super) struct ModernWatch {
    negotiation: Negotiation,
    pub signal: oneshot::Receiver<&'static str>,
}

impl Drop for ModernWatch {
    fn drop(&mut self) {
        self.negotiation
            .0
            .lock()
            .expect("MCP negotiation lock poisoned")
            .modern_watch = None;
    }
}

impl Negotiation {
    pub(super) fn watch_modern(&self, task_id: &str, created_at: &str) -> ModernWatch {
        let (send, signal) = oneshot::channel();
        let mut state = self.0.lock().expect("MCP negotiation lock poisoned");
        debug_assert!(state.modern_watch.is_none());
        state.modern_watch = Some(watch::Subscriber::new(task_id, created_at, send));
        ModernWatch {
            negotiation: self.clone(),
            signal,
        }
    }

    pub(super) fn modern_watch_end(&self, id: &RequestId, result: &Value) -> &'static str {
        let state = self.0.lock().expect("MCP negotiation lock poisoned");
        state
            .modern_watch
            .as_ref()
            .filter(|watch| watch.id.as_ref() == Some(id))
            .map_or("invalid_stream", |watch| watch.end(result))
    }

    pub(super) fn watch_status(&self, task_id: String, created_at: String) -> StatusWatch {
        let (send, signal) = oneshot::channel();
        let mut state = self.0.lock().expect("legacy MCP negotiation lock poisoned");
        // Host owns one request lease, so there can only be one active watch.
        debug_assert!(state.watch.is_none());
        state.watch = Some(WatchTarget {
            task_id,
            created_at,
            signal: Some(send),
        });
        StatusWatch {
            negotiation: self.clone(),
            signal,
        }
    }

    fn observe_notification(&self, notice: &ServerNotification) {
        let ServerNotification::CustomNotification(notice) = notice else {
            return;
        };
        if notice.method != "notifications/tasks/status" {
            return;
        }
        let Some(params) = &notice.params else { return };
        let mut state = self.0.lock().expect("legacy MCP negotiation lock poisoned");
        if let Some(watch) = &mut state.watch
            && params["taskId"].as_str() == Some(&watch.task_id)
            && params["createdAt"].as_str() == Some(&watch.created_at)
            && matches!(
                params["status"].as_str(),
                Some("completed" | "failed" | "cancelled" | "input_required")
            )
            && let Some(signal) = watch.signal.take()
        {
            let _ = signal.send(());
        }
    }

    pub fn registration(&self) -> Result<Option<Registration>, String> {
        let mut state = self.0.lock().expect("legacy MCP negotiation lock poisoned");
        if state.invalid {
            return Err("MCP server returned invalid legacy Tasks declarations".into());
        }
        Ok(state.registration.take().map(|registration| {
            // Keep only the negotiated capabilities for future wire replies.
            state.registration = Some(Registration {
                tools: BTreeMap::new(),
                ..registration
            });
            registration
        }))
    }

    pub(super) fn prepare(
        &self,
        mut message: TxJsonRpcMessage<RoleClient>,
    ) -> (TxJsonRpcMessage<WireClient>, Option<RequestId>) {
        let JsonRpcMessage::Request(request) = &mut message else {
            return (message, None);
        };
        let mut state = self.0.lock().expect("legacy MCP negotiation lock poisoned");
        let capture = match &request.request {
            ClientRequest::CustomRequest(custom) if custom.method == watch::METHOD => {
                if let Some(watch) = &mut state.modern_watch {
                    watch.id = Some(request.id.clone());
                }
                None
            }
            ClientRequest::InitializeRequest(init) => {
                if init.params.protocol_version.as_str() == "2025-11-25" {
                    let mut params = serde_json::to_value(&init.params)
                        .expect("SDK initialize parameters serialize");
                    if let Some(capabilities) = params["capabilities"].as_object_mut() {
                        // The modern extension identifier implies a different
                        // wire protocol. It is sent only on modern requests.
                        capabilities.remove("extensions");
                    }
                    params["capabilities"]["tasks"] = json!({});
                    params["capabilities"]["elicitation"] = json!({"form":{}});
                    request.request = ClientRequest::CustomRequest(CustomRequest::new(
                        "initialize",
                        Some(params),
                    ));
                }
                Some(Capture::Initialize)
            }
            ClientRequest::ListToolsRequest(_) if state.registration.is_some() => {
                Some(Capture::Tools)
            }
            ClientRequest::CustomRequest(custom)
                if state.registration.as_ref().is_some_and(|r| r.call)
                    && matches!(
                        custom.method.as_str(),
                        "tools/call" | "tasks/get" | "tasks/result" | "tasks/cancel"
                    ) =>
            {
                Some(Capture::RawTask)
            }
            _ => None,
        };
        let tracked = if let Some(capture) = capture {
            // Requests are normally serialized by Host. Bound state even if
            // transport errors prevent pending entries from being retired.
            if state.pending.len() < 8 {
                state.pending.insert(request.id.clone(), capture);
                Some(request.id.clone())
            } else {
                state.invalid = true;
                None
            }
        } else {
            None
        };
        (message, tracked)
    }

    pub(super) fn receive(
        &self,
        message: RxJsonRpcMessage<WireClient>,
    ) -> Option<RxJsonRpcMessage<RoleClient>> {
        Some(match message {
            JsonRpcMessage::Request(request) => JsonRpcMessage::Request(request),
            JsonRpcMessage::Notification(notice) => {
                let WireNotification { raw, typed } = notice.notification;
                if let Some(method) = raw["method"].as_str()
                    && let Some(watch) = &mut self
                        .0
                        .lock()
                        .expect("MCP negotiation lock poisoned")
                        .modern_watch
                {
                    watch.notify(method, &raw["params"]);
                }
                self.observe_notification(&typed);
                JsonRpcMessage::notification(typed)
            }
            JsonRpcMessage::Error(error) => {
                if let Some(id) = &error.id {
                    self.0
                        .lock()
                        .expect("legacy MCP negotiation lock poisoned")
                        .pending
                        .remove(id);
                }
                JsonRpcMessage::Error(error)
            }
            JsonRpcMessage::Response(reply) => {
                let mut state = self.0.lock().expect("legacy MCP negotiation lock poisoned");
                let capture = state.capture_reply(&reply.id, &reply.result)
                    || state
                        .modern_watch
                        .as_ref()
                        .is_some_and(|watch| watch.id.as_ref() == Some(&reply.id));
                drop(state);
                let result = if capture {
                    ServerResult::CustomResult(CustomResult::new(reply.result))
                } else {
                    serde_json::from_value(reply.result).ok()?
                };
                JsonRpcMessage::response(result, reply.id)
            }
        })
    }

    /// On a POST response stream the SDK parses event data again. Keep just
    /// the correlated old Tasks payload until its typed envelope is delivered.
    pub(super) fn observe_http_event(&self, data: &str, expected: &RequestId) {
        let Ok(message) = serde_json::from_str::<RxJsonRpcMessage<WireClient>>(data) else {
            return;
        };
        match message {
            JsonRpcMessage::Response(reply) if &reply.id == expected => {
                let mut state = self.0.lock().expect("legacy MCP negotiation lock poisoned");
                if state.capture_reply(&reply.id, &reply.result) && state.http_replies.len() < 8 {
                    state.http_replies.entry(reply.id).or_insert(reply.result);
                }
            }
            JsonRpcMessage::Error(error) if error.id.as_ref() == Some(expected) => {
                let _ = self.receive(JsonRpcMessage::Error(error));
            }
            _ => {}
        }
    }

    pub(super) fn complete_http_reply(&self, message: &mut RxJsonRpcMessage<RoleClient>) {
        if let JsonRpcMessage::Notification(notice) = message {
            self.observe_notification(&notice.notification);
        }
        if let JsonRpcMessage::Response(reply) = message
            && let Some(result) = self
                .0
                .lock()
                .expect("legacy MCP negotiation lock poisoned")
                .http_replies
                .remove(&reply.id)
        {
            reply.result = ServerResult::CustomResult(CustomResult::new(result));
        }
    }
}

impl State {
    fn capture_reply(&mut self, id: &RequestId, result: &Value) -> bool {
        match self.pending.remove(id) {
            Some(Capture::Initialize) => self.initialize(result),
            Some(Capture::Tools) => self.tools(result),
            Some(Capture::RawTask) => return true,
            None => {}
        }
        false
    }

    fn initialize(&mut self, result: &Value) {
        if result["protocolVersion"].as_str() != Some("2025-11-25") {
            return;
        }
        let tasks = &result["capabilities"]["tasks"];
        self.registration = Some(Registration {
            call: tasks["requests"]["tools"]["call"].is_object(),
            cancel: tasks["cancel"].is_object(),
            tools: BTreeMap::new(),
        });
    }

    fn tools(&mut self, result: &Value) {
        let Some(registration) = self.registration.as_mut() else {
            return;
        };
        let Some(tools) = result["tools"].as_array() else {
            return;
        };
        for tool in tools {
            let Some(support) = tool["execution"].get("taskSupport") else {
                continue;
            };
            let Some(name) = tool["name"].as_str().filter(|name| name.len() <= 256) else {
                continue;
            };
            let Ok(support) = TaskSupport::deserialize(support) else {
                self.invalid = true;
                continue;
            };
            if registration.tools.len() >= 256
                || registration
                    .tools
                    .insert(name.to_owned(), support)
                    .is_some()
            {
                self.invalid = true;
                break;
            }
        }
    }
}

pub(super) struct Stdio<T> {
    pub inner: T,
    pub negotiation: Negotiation,
}

impl<T: Transport<WireClient>> Transport<RoleClient> for Stdio<T> {
    type Error = T::Error;

    fn send(
        &mut self,
        message: TxJsonRpcMessage<RoleClient>,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send + 'static {
        self.inner.send(self.negotiation.prepare(message).0)
    }

    async fn receive(&mut self) -> Option<RxJsonRpcMessage<RoleClient>> {
        self.negotiation.receive(self.inner.receive().await?)
    }

    async fn close(&mut self) -> Result<(), Self::Error> {
        self.inner.close().await
    }
}
