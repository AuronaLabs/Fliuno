//! Short, single-task subscriptions. A notification only triggers a read.
use crate::{host::McpService, http, legacy_wire::Negotiation};
use rmcp::{
    ServiceError,
    model::{ClientRequest, CustomRequest, RequestId, ServerResult, TaskStatus},
    service::PeerRequestOptions,
};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::{sync::oneshot, time::Instant};

pub(crate) const METHOD: &str = "subscriptions/listen";
pub(crate) const ACK: &str = "notifications/subscriptions/acknowledged";
pub(crate) const SUBSCRIPTION_ID: &str = "io.modelcontextprotocol/subscriptionId";
pub(crate) const MAX_FRAMES: usize = 16;

#[derive(Clone)]
pub(crate) enum Endpoint {
    Http(http::task_watch::Endpoint),
    Stdio(Negotiation),
}

impl Endpoint {
    pub(super) async fn observe(
        &self,
        service: &McpService,
        task_id: &str,
        created_at: &str,
        deadline: Instant,
    ) -> Result<&'static str, ServiceError> {
        match self {
            Self::Http(endpoint) => {
                let peer = service
                    .peer_info()
                    .ok_or(ServiceError::UnexpectedResponse)?;
                Ok(endpoint
                    .observe(&peer.protocol_version, task_id, created_at, deadline)
                    .await)
            }
            Self::Stdio(negotiation) => {
                observe_stdio(negotiation, service, task_id, created_at, deadline).await
            }
        }
    }
}

async fn observe_stdio(
    negotiation: &Negotiation,
    service: &McpService,
    task_id: &str,
    created_at: &str,
    deadline: Instant,
) -> Result<&'static str, ServiceError> {
    let mut watch = negotiation.watch_modern(task_id, created_at);
    let request = ClientRequest::CustomRequest(CustomRequest::new(
        METHOD,
        Some(json!({"notifications":{"taskIds":[task_id]}})),
    ));
    let mut handle = service
        .send_cancellable_request(request, PeerRequestOptions::no_options())
        .await?;
    let outcome = tokio::select! {
        biased;
        signal = &mut watch.signal => signal.map_err(|_| ServiceError::TransportClosed)?,
        response = &mut handle.rx => {
            return match response.map_err(|_| ServiceError::TransportClosed)? {
                Ok(ServerResult::CustomResult(result)) => {
                    Ok(negotiation.modern_watch_end(&handle.id, &result.0))
                }
                Ok(_) => Ok("invalid_stream"),
                Err(ServiceError::McpError(_)) => Ok("rejected"),
                Err(error) => Err(error),
            };
        }
        _ = tokio::time::sleep_until(deadline) => "elapsed",
    };
    // Stop only this JSON-RPC request. Task cancellation is a separate action.
    if matches!(
        handle.rx.try_recv(),
        Err(oneshot::error::TryRecvError::Empty)
    ) {
        handle
            .cancel(Some("Fliuno task watch ended".into()))
            .await?;
    }
    Ok(outcome)
}

pub(crate) fn accept_filter(params: &Value, task_id: &str) -> Result<(), &'static str> {
    let Some(filter) = params["notifications"].as_object() else {
        return Err("invalid_stream");
    };
    match filter.get("taskIds") {
        None if filter.is_empty() => Err("declined"),
        Some(Value::Array(ids)) if filter.len() == 1 && ids.is_empty() => Err("declined"),
        Some(Value::Array(ids)) if filter.len() == 1 && ids.len() == 1 && ids[0] == task_id => {
            Ok(())
        }
        _ => Err("invalid_stream"),
    }
}

pub(crate) fn read_status(
    method: &str,
    params: &Value,
    task_id: &str,
    created_at: &str,
) -> Result<(), &'static str> {
    if method != "notifications/tasks"
        || params["taskId"] != task_id
        || params["createdAt"] != created_at
    {
        return Err("invalid_stream");
    }
    match serde_json::from_value::<TaskStatus>(params["status"].clone()) {
        Ok(TaskStatus::Working) => Ok(()),
        Ok(_) => Err("notified"),
        Err(_) => Err("invalid_stream"),
    }
}

pub(crate) struct Subscriber {
    pub id: Option<RequestId>,
    task_id: String,
    created_at: String,
    frames: Option<usize>, // None until the peer acknowledges the task filter.
    signal: Option<oneshot::Sender<&'static str>>,
}

impl Subscriber {
    pub fn new(task_id: &str, created_at: &str, signal: oneshot::Sender<&'static str>) -> Self {
        Self {
            id: None,
            task_id: task_id.into(),
            created_at: created_at.into(),
            frames: None,
            signal: Some(signal),
        }
    }

    pub fn notify(&mut self, method: &str, params: &Value) {
        if self.signal.is_none() || !self.matches_id(&params["_meta"][SUBSCRIPTION_ID]) {
            return;
        }
        let result = if let Some(frames) = &mut self.frames {
            *frames += 1;
            read_status(method, params, &self.task_id, &self.created_at).and({
                if *frames >= MAX_FRAMES {
                    Err("notification_limit")
                } else {
                    Ok(())
                }
            })
        } else if method == ACK {
            accept_filter(params, &self.task_id).map(|()| self.frames = Some(0))
        } else {
            Err("invalid_stream")
        };
        if let Err(outcome) = result
            && let Some(signal) = self.signal.take()
        {
            let _ = signal.send(outcome);
        }
    }

    pub fn end(&self, result: &Value) -> &'static str {
        if self.frames.is_some()
            && result["resultType"] == "complete"
            && self.matches_id(&result["_meta"][SUBSCRIPTION_ID])
        {
            "ended"
        } else {
            "invalid_stream"
        }
    }

    fn matches_id(&self, id: &Value) -> bool {
        self.id
            .as_ref()
            .is_some_and(|expected| RequestId::deserialize(id).is_ok_and(|id| &id == expected))
    }
}
