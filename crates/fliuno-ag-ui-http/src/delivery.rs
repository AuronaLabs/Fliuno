//! Pull small encoded pages in response to demand from the HTTP body.
use super::{Gateway, POLL_INTERVAL, Status, replay_status, task_poll::TaskWatch};
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
    through: u64,
    watch: Option<TaskWatch>,
    _permit: OwnedSemaphorePermit,
}

impl<R: BufRead + Send + 'static, W: Write + Send + 'static> Delivery<R, W> {
    pub(super) fn new(
        gateway: Arc<Gateway<R, W>>,
        replay: AgUiSseReplay,
        through: u64,
        task: Option<String>,
        permit: OwnedSemaphorePermit,
    ) -> Self {
        let watch = task.map(|id| gateway.tasks.watch(id));
        Self {
            gateway,
            replay: Some(replay),
            heartbeat: Instant::now(),
            through,
            watch,
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
            let replay = self.replay.as_mut().expect("delivery owns its replay");
            if matches!(
                replay.run_phase(),
                Some(AgUiRunPhase::Finished | AgUiRunPhase::Interrupted)
            ) {
                return Ok(None);
            }
            loop {
                sleep(POLL_INTERVAL).await;
                let through = match &self.watch {
                    Some(watch) => self.gateway.tasks.poll(&self.gateway, watch).await?,
                    None => self.gateway.poll_watermark(self.through).await?,
                };
                // A replay initialized from the global watermark can be ahead
                // of this task's latest sequence until its next event commits.
                if through > self.through {
                    replay.advance_through(through).map_err(replay_status)?;
                    self.through = through;
                    break;
                }
                if self.heartbeat.elapsed() >= Duration::from_secs(10) {
                    self.heartbeat = Instant::now();
                    return Ok(Some(Bytes::from_static(b": keep-alive\n\n")));
                }
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
