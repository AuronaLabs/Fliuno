//! An embeddable HTTP transport for Fliuno's AG-UI text profile.
mod delivery;

use axum::{
    Router,
    body::{Body, to_bytes},
    extract::{Path, Request, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use fliuno_client::{
    Client, Error,
    ag_ui::{AG_UI_SSE_CONTENT_TYPE, AgUiRunError, AgUiSseError, AgUiSseFeedError, AgUiSseReplay},
};
use fliuno_protocol::MAX_APP_REQUEST_BYTES;
use std::{
    io::{BufRead, Write},
    sync::Arc,
    time::Duration,
};
use tokio::{
    sync::{Mutex, Semaphore},
    time::{Instant, timeout},
};

const POLL_INTERVAL: Duration = Duration::from_millis(150);

type Authorize = dyn Fn(&HeaderMap) -> Option<String> + Send + Sync;

struct Gateway<R, W> {
    client: Arc<Mutex<Client<R, W>>>,
    authorize: Box<Authorize>,
    streams: Arc<Semaphore>,
    watermark: Mutex<Option<(Instant, u64)>>,
}

/// Mount `POST /run` and `GET /runs/{run_id}` on the host's HTTP listener.
/// The authorization callback authenticates each request and selects its local
/// session. Return `None` to reject the request. It runs before any kernel RPC.
/// Share the client with the host's approval handler through the same mutex.
pub fn router<R, W>(
    client: Arc<Mutex<Client<R, W>>>,
    authorize: impl Fn(&HeaderMap) -> Option<String> + Send + Sync + 'static,
) -> Router
where
    R: BufRead + Send + 'static,
    W: Write + Send + 'static,
{
    let gateway = Arc::new(Gateway {
        client,
        authorize: Box::new(authorize),
        streams: Arc::new(Semaphore::new(16)),
        watermark: Mutex::new(None),
    });
    Router::new()
        .route("/run", post(start::<R, W>))
        .route("/runs/{run_id}", get(attach::<R, W>))
        .with_state(gateway)
}

type Status<T> = Result<T, StatusCode>;

impl<R: BufRead + Send + 'static, W: Write + Send + 'static> Gateway<R, W> {
    async fn rpc<T: Send + 'static>(
        &self,
        call: impl FnOnce(&mut Client<R, W>) -> Status<T> + Send + 'static,
    ) -> Status<T> {
        let mut client = self.client.clone().lock_owned().await;
        tokio::task::spawn_blocking(move || call(&mut client))
            .await
            .map_err(|_| StatusCode::BAD_GATEWAY)?
    }

    async fn poll_watermark(&self, minimum: u64) -> Status<u64> {
        // Coalesce overlapping idle readers. Initial requests still use a fresh
        // sample to validate a reconnect against the current store.
        let mut sample = self.watermark.lock().await;
        if let Some((time, through)) = *sample
            && time.elapsed() < POLL_INTERVAL
            && through >= minimum
        {
            return Ok(through);
        }
        let through = self
            .rpc(|client| client.event_watermark(|_| {}).map_err(rpc_status))
            .await?;
        if through < minimum {
            return Err(StatusCode::BAD_GATEWAY);
        }
        *sample = Some((Instant::now(), through));
        Ok(through)
    }
}

async fn start<R: BufRead + Send + 'static, W: Write + Send + 'static>(
    State(gateway): State<Arc<Gateway<R, W>>>,
    request: Request,
) -> Status<Response> {
    let session = (gateway.authorize)(request.headers()).ok_or(StatusCode::UNAUTHORIZED)?;
    if request.headers().contains_key("last-event-id") {
        return Err(StatusCode::BAD_REQUEST);
    }
    let is_json = request
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|header| header.to_str().ok())
        .is_some_and(|mime| {
            mime.split(';')
                .next()
                .unwrap_or_default()
                .trim()
                .eq_ignore_ascii_case("application/json")
        });
    if !is_json {
        return Err(StatusCode::UNSUPPORTED_MEDIA_TYPE);
    }
    let permit = gateway
        .streams
        .clone()
        .try_acquire_owned()
        .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
    let body = timeout(
        Duration::from_secs(10),
        to_bytes(request.into_body(), MAX_APP_REQUEST_BYTES),
    )
    .await
    .map_err(|_| StatusCode::REQUEST_TIMEOUT)?
    .map_err(|_| StatusCode::PAYLOAD_TOO_LARGE)?;
    let (replay, through) = gateway
        .rpc(move |client| {
            let run = client
                .start_ag_ui_run(&session, &body, |_| {})
                .map_err(run_status)?;
            let through = client.event_watermark(|_| {}).map_err(rpc_status)?;
            let replay = run.replay_sse(through, None).map_err(replay_status)?;
            Ok((replay, through))
        })
        .await?;
    respond(gateway, replay, through, permit).await
}

async fn attach<R: BufRead + Send + 'static, W: Write + Send + 'static>(
    State(gateway): State<Arc<Gateway<R, W>>>,
    Path(run_id): Path<String>,
    headers: HeaderMap,
) -> Status<Response> {
    let session = (gateway.authorize)(&headers).ok_or(StatusCode::UNAUTHORIZED)?;
    let last_id = headers
        .get("last-event-id")
        .map(|value| {
            value
                .to_str()
                .map(str::to_owned)
                .map_err(|_| StatusCode::BAD_REQUEST)
        })
        .transpose()?;
    let permit = gateway
        .streams
        .clone()
        .try_acquire_owned()
        .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
    let (replay, through) = gateway
        .rpc(move |client| {
            let run = client
                .attach_ag_ui_run(&session, &run_id, |_| {})
                .map_err(run_status)?;
            let through = client.event_watermark(|_| {}).map_err(rpc_status)?;
            let replay = run
                .replay_sse(through, last_id.as_deref())
                .map_err(replay_status)?;
            Ok((replay, through))
        })
        .await?;
    respond(gateway, replay, through, permit).await
}

async fn respond<R: BufRead + Send + 'static, W: Write + Send + 'static>(
    gateway: Arc<Gateway<R, W>>,
    replay: AgUiSseReplay,
    through: u64,
    permit: tokio::sync::OwnedSemaphorePermit,
) -> Status<Response> {
    let mut delivery = delivery::Delivery::new(gateway, replay, through, permit);
    // Validate the resume position before committing response headers.
    let first = loop {
        let chunk = delivery.available().await?;
        if chunk.as_ref().is_none_or(|chunk| !chunk.is_empty()) {
            break chunk;
        }
    };
    let stream = delivery::body(first, delivery);
    Ok((
        [
            (header::CONTENT_TYPE, AG_UI_SSE_CONTENT_TYPE),
            (header::CACHE_CONTROL, "no-store"),
        ],
        Body::from_stream(stream),
    )
        .into_response())
}

fn run_status(error: AgUiRunError) -> StatusCode {
    match error {
        AgUiRunError::WrongThread => StatusCode::NOT_FOUND,
        AgUiRunError::InputTooLarge => StatusCode::PAYLOAD_TOO_LARGE,
        AgUiRunError::Rpc(error) => rpc_status(error),
        AgUiRunError::InvalidResult => StatusCode::BAD_GATEWAY,
        _ => StatusCode::BAD_REQUEST,
    }
}

fn rpc_status(error: Error) -> StatusCode {
    match error {
        Error::Rpc { code: -32004, .. } => StatusCode::NOT_FOUND,
        Error::Rpc { code: -32009, .. } => StatusCode::CONFLICT,
        Error::RequestTooLarge => StatusCode::PAYLOAD_TOO_LARGE,
        _ => StatusCode::BAD_GATEWAY,
    }
}

fn replay_status(error: AgUiSseFeedError) -> StatusCode {
    match error {
        AgUiSseFeedError::Sse(AgUiSseError::InvalidResumeId | AgUiSseError::UnknownResumeId) => {
            StatusCode::BAD_REQUEST
        }
        AgUiSseFeedError::Rpc(error) => rpc_status(error),
        _ => StatusCode::BAD_GATEWAY,
    }
}
