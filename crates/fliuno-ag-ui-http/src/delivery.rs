//! Pull small encoded pages in response to demand from the HTTP body.
use super::{Gateway, Status, replay_status, rpc_status};
use axum::body::Bytes;
use fliuno_client::ag_ui::{AgUiRunPhase, AgUiSseReplay};
use futures_util::{Stream, stream};
use std::{
    io::{self, BufRead, Write},
    sync::Arc,
    time::Duration,
};
use tokio::{
    sync::OwnedSemaphorePermit,
    time::{Instant, sleep},
};

pub(super) struct Delivery<R, W> {
    gateway: Arc<Gateway<R, W>>,
    replay: Option<AgUiSseReplay>,
    heartbeat: Instant,
    _permit: OwnedSemaphorePermit,
}

impl<R: BufRead + Send + 'static, W: Write + Send + 'static> Delivery<R, W> {
    pub(super) fn new(
        gateway: Arc<Gateway<R, W>>,
        replay: AgUiSseReplay,
        permit: OwnedSemaphorePermit,
    ) -> Self {
        Self {
            gateway,
            replay: Some(replay),
            heartbeat: Instant::now(),
            _permit: permit,
        }
    }

    pub(super) async fn available(&mut self) -> Status<Option<Bytes>> {
        let mut replay = self.replay.take().expect("delivery owns its replay");
        let (replay, bytes) = self
            .gateway
            .rpc(move |client| {
                // Yield the connection between pages even when reconnecting far into history.
                let chunk = replay.next_chunk(client, |_| {}).map_err(replay_status)?;
                Ok((replay, chunk))
            })
            .await?;
        self.replay = Some(replay);
        Ok(bytes.map(Bytes::from))
    }

    async fn next(&mut self) -> Status<Option<Bytes>> {
        loop {
            if let Some(chunk) = self.available().await? {
                if !chunk.is_empty() {
                    self.heartbeat = Instant::now();
                    return Ok(Some(chunk));
                }
                continue;
            }
            let mut replay = self.replay.take().expect("delivery owns its replay");
            if matches!(
                replay.run_phase(),
                Some(AgUiRunPhase::Finished | AgUiRunPhase::Interrupted)
            ) {
                return Ok(None);
            }
            sleep(Duration::from_millis(150)).await;
            self.replay = Some(
                self.gateway
                    .rpc(move |client| {
                        let through = client.event_watermark(|_| {}).map_err(rpc_status)?;
                        replay.advance_through(through).map_err(replay_status)?;
                        Ok(replay)
                    })
                    .await?,
            );
            if self.heartbeat.elapsed() >= Duration::from_secs(10) {
                self.heartbeat = Instant::now();
                return Ok(Some(Bytes::from_static(b": keep-alive\n\n")));
            }
        }
    }
}

pub(super) fn body<R: BufRead + Send + 'static, W: Write + Send + 'static>(
    first: Option<Bytes>,
    delivery: Delivery<R, W>,
) -> impl Stream<Item = io::Result<Bytes>> + Send + 'static {
    stream::try_unfold((first, delivery), |(first, mut delivery)| async move {
        if let Some(chunk) = first.filter(|bytes| !bytes.is_empty()) {
            return Ok(Some((chunk, (None, delivery))));
        }
        let chunk = delivery
            .next()
            .await
            .map_err(|status| io::Error::other(status.as_str()))?;
        Ok(chunk.map(|chunk| (chunk, (None, delivery))))
    })
}
