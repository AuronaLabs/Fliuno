//! One live legacy tasks/result exchange. The durable input token addresses
//! only this connection; losing the exchange never causes a second dispatch.
use super::{McpService, Scope, ServiceError};
use fliuno_core::{ToolContext, ToolError, json_size};
use fliuno_protocol::{MAX_TOOL_INPUT_BYTES, MAX_TOOL_INPUT_ROUNDS};
use rmcp::{
    ErrorData, RoleClient,
    model::{
        ClientRequest, CustomRequest, ElicitRequest, ElicitRequestParams, ElicitResult,
        InputRequest, MetaObject, ProtocolVersion, RequestId, ServerResult,
    },
    service::{PeerRequestOptions, RequestContext},
};
use serde_json::{Value, json};
use std::{collections::HashSet, sync::Mutex, time::Duration};
use tokio::{
    sync::{mpsc, oneshot},
    task::JoinHandle,
    time::Instant,
};

pub(super) const RELATED_TASK: &str = "io.modelcontextprotocol/related-task";
type RawResult = Result<Option<Value>, ServiceError>;
type Outcome = Result<Option<Result<Value, ToolError>>, ServiceError>;

#[derive(PartialEq, Eq)]
pub(super) struct Owner {
    scope: Scope,
    local_task: String,
    call_id: String,
}

impl Owner {
    pub fn new(context: &ToolContext<'_>, call_id: &str) -> Self {
        Self {
            scope: Scope::from(context),
            local_task: context.task_id.into(),
            call_id: call_id.into(),
        }
    }
}

struct Form {
    token: String,
    request_id: String,
    requests: Value,
    answer: oneshot::Sender<ElicitResult>,
}

struct Awaiting {
    forms: mpsc::Receiver<Form>,
    reply: JoinHandle<RawResult>,
}

impl Drop for Awaiting {
    fn drop(&mut self) {
        self.reply.abort();
    }
}

struct Exchange {
    task_id: String,
    owner: Owner,
    deadline: Instant,
    forms: mpsc::Sender<Form>,
    seen: HashSet<RequestId>,
    suspended: Option<(Form, Awaiting)>,
}

#[derive(Default)]
pub(crate) struct Bridge(Mutex<Option<Exchange>>);

struct Lease<'a> {
    bridge: &'a Bridge,
    suspended: bool,
}

impl Drop for Lease<'_> {
    fn drop(&mut self) {
        if !self.suspended {
            self.bridge.0.lock().expect("MCP form lock poisoned").take();
        }
    }
}

impl Bridge {
    pub(crate) fn reserved(&self) -> bool {
        let mut exchange = self.0.lock().expect("MCP form lock poisoned");
        if exchange.as_ref().is_some_and(|x| {
            x.suspended
                .as_ref()
                .is_some_and(|(_, a)| a.reply.is_finished())
        }) {
            exchange.take();
        }
        exchange.is_some()
    }

    pub(super) async fn start(
        &self,
        service: &McpService,
        task_id: String,
        owner: Owner,
        timeout: Duration,
    ) -> Outcome {
        let (sender, forms) = mpsc::channel(1);
        {
            let mut exchange = self.0.lock().expect("MCP form lock poisoned");
            if exchange.is_some() {
                return known("MCP connection already has an active form exchange");
            }
            *exchange = Some(Exchange {
                task_id: task_id.clone(),
                owner,
                deadline: Instant::now() + timeout,
                forms: sender,
                seen: HashSet::new(),
                suspended: None,
            });
        }
        let lease = Lease {
            bridge: self,
            suspended: false,
        };
        let peer = service.peer().clone();
        let reply = tokio::spawn(async move {
            let response = peer
                .send_request_with_option(
                    ClientRequest::CustomRequest(CustomRequest::new(
                        "tasks/result",
                        Some(json!({"taskId":task_id})),
                    )),
                    PeerRequestOptions::with_timeout(timeout),
                )
                .await?
                .await_response()
                .await?;
            Ok(match response {
                ServerResult::CustomResult(result) => Some(result.0),
                _ => None,
            })
        });
        lease.next(Awaiting { forms, reply }).await
    }

    pub(super) async fn resume(
        &self,
        owner: Owner,
        task_id: &str,
        token: &str,
        responses: &Value,
    ) -> Outcome {
        let (form, awaiting) = {
            let mut guard = self.0.lock().expect("MCP form lock poisoned");
            let Some(exchange) = guard.as_mut().filter(|x| {
                x.owner == owner
                    && x.task_id == task_id
                    && Instant::now() < x.deadline
                    && x.suspended.as_ref().is_some_and(|(form, awaiting)| {
                        form.token == token && !awaiting.reply.is_finished()
                    })
            }) else {
                return Ok(Some(Err(ToolError::OutcomeUnknown(
                    "legacy MCP input no longer has its original live request".into(),
                ))));
            };
            let (form, _) = exchange.suspended.as_ref().expect("checked active form");
            if let Err(error) = crate::validate_input_responses(&form.requests, responses) {
                return known(&error);
            }
            exchange.suspended.take().expect("checked active form")
        };
        let lease = Lease {
            bridge: self,
            suspended: false,
        };
        let mut response: ElicitResult =
            serde_json::from_value(responses[&form.request_id].clone())
                .expect("validated elicitation response");
        // Never forward caller-provided response metadata.
        response.meta = Some(MetaObject::from(serde_json::Map::from_iter([(
            RELATED_TASK.to_owned(),
            json!({"taskId":task_id}),
        )])));
        if form.answer.send(response).is_err() {
            return Ok(None);
        }
        lease.next(awaiting).await
    }

    pub(crate) async fn request_form(
        &self,
        request: ElicitRequestParams,
        context: RequestContext<RoleClient>,
    ) -> Result<ElicitResult, ErrorData> {
        let rejected = || {
            ErrorData::invalid_params(
                "form is not bound to a live legacy MCP task result request",
                None,
            )
        };
        if context
            .peer
            .peer_info()
            .is_none_or(|peer| peer.protocol_version != ProtocolVersion::V_2025_11_25)
        {
            return Err(rejected());
        }
        let related = context
            .meta
            .get(RELATED_TASK)
            .and_then(|v| v["taskId"].as_str());
        if related.is_none() || matches!(&context.id, RequestId::String(id) if id.len() > 512) {
            return Err(rejected());
        }
        let ElicitRequestParams::FormElicitationParams {
            message,
            requested_schema,
            ..
        } = request
        else {
            return Err(rejected());
        };
        let request_id = uuid::Uuid::new_v4().to_string();
        let sanitized = InputRequest::Elicitation(ElicitRequest::new(
            ElicitRequestParams::FormElicitationParams {
                meta: None,
                message,
                requested_schema,
            },
        ));
        let requests = json!({request_id.clone():sanitized});
        if !json_size(&requests).is_ok_and(|bytes| bytes <= MAX_TOOL_INPUT_BYTES) {
            return Err(rejected());
        }
        let (answer, response) = oneshot::channel();
        {
            let mut guard = self.0.lock().expect("MCP form lock poisoned");
            let exchange = guard
                .as_mut()
                .filter(|exchange| {
                    related == Some(&exchange.task_id)
                        && Instant::now() < exchange.deadline
                        && exchange.seen.len() < usize::from(MAX_TOOL_INPUT_ROUNDS)
                        && !exchange.seen.contains(&context.id)
                })
                .ok_or_else(rejected)?;
            exchange
                .forms
                .try_send(Form {
                    token: uuid::Uuid::new_v4().to_string(),
                    request_id,
                    requests,
                    answer,
                })
                .map_err(|_| rejected())?;
            exchange.seen.insert(context.id.clone());
        }
        tokio::select! {
            biased;
            _ = context.ct.cancelled() => Err(rejected()),
            response = response => response.map_err(|_| rejected()),
        }
    }
}

impl Lease<'_> {
    async fn next(mut self, mut awaiting: Awaiting) -> Outcome {
        tokio::select! {
            biased;
            response = &mut awaiting.reply => response.unwrap_or(Ok(None)).map(|result| result.map(Ok)),
            form = awaiting.forms.recv() => {
                let Some(form) = form else { return Ok(None) };
                let error = ToolError::InputRequired {
                    requests: form.requests.clone(), request_state: Some(form.token.clone()),
                };
                self.bridge.0.lock().expect("MCP form lock poisoned").as_mut()
                    .expect("lease owns live exchange").suspended = Some((form, awaiting));
                self.suspended = true;
                Ok(Some(Err(error)))
            }
        }
    }
}

fn known(message: &str) -> Outcome {
    Ok(Some(Err(ToolError::Failed(message.to_owned()))))
}
