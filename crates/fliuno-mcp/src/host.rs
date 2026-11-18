//! One serialized request owner per MCP connection. Queue cancellation never
//! affects the current owner; request/transport uncertainty closes the connection.
use fliuno_core::{CancellationToken, ToolError};
use rmcp::{RoleClient, ServiceError, model::ResultType, service::RunningService};
use std::{future::Future, pin::Pin, time::Duration};
use tokio::{runtime::Runtime, sync::Mutex};

type Response<'a, T> = Pin<Box<dyn Future<Output = Result<Option<T>, ServiceError>> + 'a>>;

/// Untagged SDK results can have a final payload shape and a non-final marker
/// simultaneously. Missing markers preserve pre-2026 protocol compatibility.
pub(crate) fn is_complete(result_type: &Option<ResultType>) -> bool {
    result_type
        .as_ref()
        .is_none_or(|kind| kind == &ResultType::COMPLETE)
}

pub(crate) struct Host {
    pub authority: String,
    pub service: Mutex<RunningService<RoleClient, ()>>,
    pub runtime: Runtime,
    pub timeout: Duration,
}

impl Host {
    /// The operation sends one request and returns None for an unsupported
    /// continuation or response kind. It must never drive MRTR rounds itself.
    pub fn request<T: 'static>(
        &self,
        cancellation: &CancellationToken,
        operation: impl for<'a> FnOnce(&'a RunningService<RoleClient, ()>) -> Response<'a, T>,
    ) -> Result<T, ToolError> {
        self.runtime.block_on(async {
            let deadline = tokio::time::Instant::now() + self.timeout;
            let mut service = tokio::select! {
                biased;
                _ = cancellation.cancelled() => return Err(ToolError::Failed(
                    "MCP call cancelled before dispatch; no request sent".into(),
                )),
                lock = tokio::time::timeout_at(deadline, self.service.lock()) => {
                    lock.map_err(|_| ToolError::Failed(
                        "MCP queue wait timed out; no request sent".into(),
                    ))?
                },
            };
            if service.is_closed() {
                return Err(ToolError::Failed("MCP host is closed".into()));
            }
            if cancellation.is_cancelled() {
                return Err(ToolError::Failed(
                    "MCP call cancelled before dispatch; no request sent".into(),
                ));
            }
            let response = tokio::select! {
                biased;
                _ = cancellation.cancelled() => None,
                response = tokio::time::timeout_at(deadline, operation(&service)) => Some(response),
            };
            let message = match response {
                Some(Ok(Ok(Some(result)))) => return Ok(result),
                Some(Ok(Ok(None))) => {
                    "MCP continuation or response kind is unsupported; result unknown; host stopped"
                }
                Some(Ok(Err(_))) => "MCP execution failed; result unknown; host stopped",
                Some(Err(_)) => "MCP execution timed out; result unknown; host stopped",
                None => "MCP execution cancelled; result unknown; host stopped",
            };
            // Bound graceful transport cleanup separately from the operation.
            // The SDK marks itself closed before awaiting its cleanup worker.
            let _ = service.close_with_timeout(Duration::from_secs(5)).await;
            Err(ToolError::OutcomeUnknown(message.into()))
        })
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        self.runtime.block_on(async {
            let _ = self
                .service
                .get_mut()
                .close_with_timeout(Duration::from_secs(5))
                .await;
        });
    }
}
