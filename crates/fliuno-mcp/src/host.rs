//! One serialized request owner per MCP connection. Queue cancellation never
//! affects the current owner; request/transport uncertainty closes the connection.
use fliuno_core::{
    CancellationToken, ToolError,
    diagnostics::{Operation, Scope},
};
use rmcp::{
    RoleClient, ServiceError,
    model::{ProtocolVersion, ResultType},
    service::{ClientCacheConfig, RunningService},
};
use std::{future::Future, pin::Pin, time::Duration};
use tokio::{runtime::Runtime, sync::Mutex};

type Response<'a, T> = Pin<Box<dyn Future<Output = Result<Option<T>, ServiceError>> + 'a>>;

#[derive(Clone, Copy, PartialEq, Eq)]
enum RequestMode {
    Effectful,
    ReadOnly,
    ResumeLegacyForm,
}

pub(crate) type McpService = RunningService<RoleClient, crate::client::Client>;

pub(crate) fn cache_config(version: &ProtocolVersion, authority: &str) -> ClientCacheConfig {
    if *version < ProtocolVersion::V_2026_07_28 {
        return ClientCacheConfig::disabled();
    }
    // The SDK cache belongs to this peer alone, including public entries. A
    // registration or credential change creates a new peer and empty cache.
    ClientCacheConfig::default()
        .with_private_partition(authority)
        .with_max_entries(8)
        .with_max_ttl(Duration::from_secs(300))
        .with_serve_stale_on_error(false)
}

/// Untagged SDK results can have a final payload shape and a non-final marker
/// simultaneously. Missing markers preserve pre-2026 protocol compatibility.
pub(crate) fn is_complete(result_type: &Option<ResultType>) -> bool {
    result_type
        .as_ref()
        .is_none_or(|kind| kind == &ResultType::COMPLETE)
}

pub(crate) struct Host {
    pub authority: String,
    pub service: Mutex<McpService>,
    pub runtime: Runtime,
    pub timeout: Duration,
    pub task_bindings: std::sync::Arc<crate::tasks::Bindings>,
}

impl Host {
    /// The operation sends one request and returns None for an unsupported
    /// continuation or response kind. It must never drive MRTR rounds itself.
    pub fn request<T: 'static>(
        &self,
        cancellation: &CancellationToken,
        operation: impl for<'a> FnOnce(&'a McpService) -> Response<'a, T>,
    ) -> Result<T, ToolError> {
        self.request_with_mode(cancellation, operation, RequestMode::Effectful)
    }

    /// Preserve the connection when a side-effect-free MCP method is rejected.
    /// An operation may perform bounded sequential reads under the same lease.
    pub fn request_read<T: 'static>(
        &self,
        cancellation: &CancellationToken,
        operation: impl for<'a> FnOnce(&'a McpService) -> Response<'a, T>,
    ) -> Result<T, ToolError> {
        self.request_with_mode(cancellation, operation, RequestMode::ReadOnly)
    }

    pub fn resume_legacy_form<T: 'static>(
        &self,
        cancellation: &CancellationToken,
        operation: impl for<'a> FnOnce(&'a McpService) -> Response<'a, T>,
    ) -> Result<T, ToolError> {
        self.request_with_mode(cancellation, operation, RequestMode::ResumeLegacyForm)
    }

    fn request_with_mode<T: 'static>(
        &self,
        cancellation: &CancellationToken,
        operation: impl for<'a> FnOnce(&'a McpService) -> Response<'a, T>,
        mode: RequestMode,
    ) -> Result<T, ToolError> {
        let mut span = Scope::current().and_then(|scope| scope.begin(Operation::Mcp));
        let outcome = self.runtime.block_on(async {
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
            if mode != RequestMode::ResumeLegacyForm && service.service().legacy_input.reserved() {
                return Err(ToolError::Failed(
                    "MCP connection is waiting for approved legacy task input".into(),
                ));
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
                Some(Ok(Err(ServiceError::McpError(_)))) if mode == RequestMode::ReadOnly => {
                    return Err(ToolError::Failed(
                        "MCP server rejected a read request".into(),
                    ));
                }
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
        });
        if let Some(span) = &mut span {
            span.tool(&outcome, cancellation.is_cancelled());
        }
        outcome
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
