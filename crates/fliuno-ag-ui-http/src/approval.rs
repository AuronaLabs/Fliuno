//! Host-authorized reads and decisions for a run's pending tool calls.
use super::{Gateway, Status, read_json, rpc_status, run_status};
use axum::{
    extract::{Path, Request, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
};
use fliuno_client::Client;
use fliuno_protocol::{
    ApprovalDecision, ApprovalDetails, ApprovalGetParams, ApprovalRespondParams, Event, EventKind,
};
use serde::{Deserialize, Serialize};
use std::{
    io::{BufRead, Write},
    sync::Arc,
};
use tokio::sync::OwnedSemaphorePermit;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Decision {
    decision: ApprovalDecision,
}

pub(super) async fn get<R: BufRead + Send + 'static, W: Write + Send + 'static>(
    State(gateway): State<Arc<Gateway<R, W>>>,
    Path((run_id, approval_id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Status<Response> {
    let (session, _permit) = review_access(&gateway, &headers, &approval_id)?;
    let details: ApprovalDetails = gateway
        .rpc(move |client| {
            let params = approval_params(client, &session, &run_id, approval_id)?;
            client
                .call("approval/get", &params, |_| {})
                .map_err(rpc_status)
        })
        .await?;
    json_response(&details)
}

pub(super) async fn respond<R: BufRead + Send + 'static, W: Write + Send + 'static>(
    State(gateway): State<Arc<Gateway<R, W>>>,
    Path((run_id, approval_id)): Path<(String, String)>,
    request: Request,
) -> Status<Response> {
    let (session, _permit) = review_access(&gateway, request.headers(), &approval_id)?;
    let body = read_json(request, 1024).await?;
    let Decision { decision } =
        serde_json::from_slice(&body).map_err(|_| StatusCode::BAD_REQUEST)?;
    let event = gateway
        .rpc(move |client| {
            let params = approval_params(client, &session, &run_id, approval_id)?;
            let request = ApprovalRespondParams {
                task_id: params.task_id,
                approval_id: params.approval_id,
                decision,
            };
            let event: Event = client
                .call("approval/respondDecision", &request, |_| {})
                .map_err(rpc_status)?;
            match &event.data {
                EventKind::ApprovalResolved {
                    approval_id,
                    decision: committed,
                } if event.task_id == request.task_id
                    && approval_id == &request.approval_id
                    && *committed == decision =>
                {
                    Ok(event)
                }
                _ => Err(StatusCode::BAD_GATEWAY),
            }
        })
        .await?;
    json_response(&event)
}

fn json_response(value: &impl Serialize) -> Status<Response> {
    let body = serde_json::to_vec(value).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok((
        [
            (header::CONTENT_TYPE, "application/json"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        body,
    )
        .into_response())
}

fn approval_params<R: BufRead, W: Write>(
    client: &mut Client<R, W>,
    session: &str,
    run_id: &str,
    approval_id: String,
) -> Status<ApprovalGetParams> {
    let run = client
        .attach_ag_ui_run(session, run_id, |_| {})
        .map_err(run_status)?;
    Ok(ApprovalGetParams {
        task_id: run.task().id.clone(),
        approval_id,
    })
}

fn review_access<R, W>(
    gateway: &Gateway<R, W>,
    headers: &HeaderMap,
    approval_id: &str,
) -> Status<(String, OwnedSemaphorePermit)> {
    let session = gateway
        .reviewer
        .as_ref()
        .and_then(|authorize| authorize(headers))
        .ok_or(StatusCode::UNAUTHORIZED)?;
    if approval_id.is_empty()
        || approval_id.len() > 128
        || approval_id.chars().any(char::is_control)
    {
        return Err(StatusCode::BAD_REQUEST);
    }
    let permit = gateway
        .reviews
        .clone()
        .try_acquire_owned()
        .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
    Ok((session, permit))
}
