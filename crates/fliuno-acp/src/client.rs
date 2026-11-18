use crate::{
    CancellationToken, ClientOptions, Error, Event, HostRequest, HostResponse, PermissionDecision,
    Result, filesystem, permissions,
    schema::*,
    state::{OpenGuard, Opening, PromptGuard, Shared, State, valid_session_id},
    transport,
};
use agent_client_protocol::{
    self as acp, Agent, ConnectTo, schema::ProtocolVersion, v1::ConnectionTo,
};
use std::{path::Path, path::PathBuf, sync::Arc, sync::Mutex};
use tokio::sync::{mpsc, oneshot};

mod catalog;
mod session;

/// Cloneable handle. Dropping its last owner stops the driver and owned process.
#[derive(Clone)]
pub struct Client {
    inner: Arc<Inner>,
}

struct Inner {
    connection: ConnectionTo<Agent>,
    initialized: InitializeResponse,
    shared: Arc<Shared>,
    directory: Option<PathBuf>,
    catalog: tokio::sync::Mutex<()>,
    driver: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl Drop for Inner {
    fn drop(&mut self) {
        self.shared.stop.cancel();
    }
}

/// Single consumer of a bounded live feed. Dropping it stops the connection;
/// permissions must never be approved without an observer for their context.
pub struct Events {
    receiver: mpsc::Receiver<Event>,
    shared: Arc<Shared>,
}

impl Events {
    pub async fn recv(&mut self) -> Option<Event> {
        if let Ok(event) = self.receiver.try_recv() {
            return Some(event);
        }
        tokio::select! {
            biased;
            event = self.receiver.recv() => event,
            _ = self.shared.stop.cancelled() => None,
        }
    }
}

impl Drop for Events {
    fn drop(&mut self) {
        self.shared.fail(Error::EventOverflow);
    }
}

/// An abandoned initialization must not leave an unowned agent connection alive.
struct Starting(Option<Arc<Shared>>);
impl Drop for Starting {
    fn drop(&mut self) {
        if let Some(shared) = &self.0 {
            shared.stop.cancel();
        }
    }
}

struct DriverFinished(Arc<Shared>);
impl Drop for DriverFinished {
    fn drop(&mut self) {
        self.0.fail(Error::Closed);
        self.0.finished.cancel();
    }
}

impl Client {
    /// Connect to a host-supplied SDK transport. The caller must manage authority
    /// and process lifetime. The adapter does not implicitly spawn a host process.
    pub async fn connect(
        transport: impl ConnectTo<acp::Client>,
        options: ClientOptions,
    ) -> Result<(Self, Events)> {
        Self::connect_inner(transport, options, None, None).await
    }

    /// Bounded newline JSON-RPC over arbitrary Tokio streams. Each incoming line
    /// is capped at 1 MiB before the SDK parses it; outgoing prompts are text-only.
    pub async fn connect_io(
        input: impl tokio::io::AsyncRead + Send + 'static,
        output: impl tokio::io::AsyncWrite + Send + 'static,
        options: ClientOptions,
    ) -> Result<(Self, Events)> {
        Self::connect(transport::lines(input, output), options).await
    }

    /// Stage one native executable into the current platform's private sandbox.
    /// There is no host fallback. The driver owns it through close and timeout.
    pub async fn connect_isolated(
        spec: fliuno_sandbox::LaunchSpec,
        options: ClientOptions,
    ) -> Result<(Self, Events)> {
        options.validate()?;
        let timeout = options.request_timeout;
        let launch = fliuno_sandbox::spawn_async(spec);
        // A timed-out blocking launch still drops its eventual Child result,
        // which tears down the Job/profile instead of leaking an orphan process.
        let (child, pipes) = tokio::time::timeout(timeout, launch)
            .await
            .map_err(|_| {
                eprintln!("fliuno: ACP isolated process launch timed out");
                Error::OutcomeUnknown
            })?
            .map_err(|_| Error::Transport)?;
        Self::connect_native(transport::Child::Isolated(child), pipes, options).await
    }

    /// Connect to an explicitly trusted Windows program under the current
    /// OS user's authority. The driver owns its Job and bounded stdio.
    #[cfg(windows)]
    pub async fn connect_trusted_host(
        spec: fliuno_sandbox::HostLaunchSpec,
        options: ClientOptions,
    ) -> Result<(Self, Events)> {
        options.validate()?;
        let (child, pipes) = tokio::time::timeout(
            options.request_timeout,
            fliuno_sandbox::spawn_host_async(spec),
        )
        .await
        .map_err(|_| Error::OutcomeUnknown)?
        .map_err(|_| Error::Transport)?;
        Self::connect_native(transport::Child::Trusted(child), pipes, options).await
    }

    async fn connect_native(
        child: transport::Child,
        pipes: fliuno_sandbox::AsyncStdio,
        options: ClientOptions,
    ) -> Result<(Self, Events)> {
        let (peer, input, output, directory) = transport::NativePeer::new(child, pipes);
        Self::connect_inner(
            transport::lines(input, output),
            options,
            Some(peer),
            directory,
        )
        .await
    }

    async fn connect_inner(
        transport: impl ConnectTo<acp::Client>,
        options: ClientOptions,
        peer: Option<transport::NativePeer>,
        directory: Option<PathBuf>,
    ) -> Result<(Self, Events)> {
        options.validate()?;
        let (events, receiver) = mpsc::channel(options.event_capacity);
        let shared = Arc::new(Shared {
            state: Mutex::new(State::default()),
            events,
            stop: CancellationToken::new(),
            finished: CancellationToken::new(),
            options,
        });
        let mut starting = Starting(Some(Arc::clone(&shared)));
        let (ready, initialized) = oneshot::channel();
        let updates = Arc::clone(&shared);
        let requests = Arc::clone(&shared);
        let reads = Arc::clone(&shared);
        let writes = Arc::clone(&shared);
        let worker = Arc::clone(&shared);
        let driver = tokio::spawn(async move {
            let _finished = DriverFinished(Arc::clone(&worker));
            let client = acp::Client
                .v1()
                .name("fliuno-acp")
                .on_receive_notification(
                    async move |notification: SessionNotification, _cx| {
                        updates
                            .update(notification)
                            .map_err(|_| acp::Error::invalid_params())
                    },
                    acp::on_receive_notification!(),
                )
                .on_receive_request(
                    async move |request: RequestPermissionRequest, responder, cx| {
                        let Some(waiter) = permissions::prepare(Arc::clone(&requests), request)
                        else {
                            return responder.respond(RequestPermissionResponse::new(
                                RequestPermissionOutcome::Cancelled,
                            ));
                        };
                        // Awaiting UI consent in SDK dispatch would block progress
                        // and session/cancel. The SDK owns this concurrent waiter.
                        cx.spawn(async move {
                            let outcome = waiter.resolve().await;
                            responder.respond(RequestPermissionResponse::new(outcome))
                        })
                    },
                    acp::on_receive_request!(),
                )
                .on_receive_request(
                    async move |request: ReadTextFileRequest, responder, cx| {
                        if !reads.options.filesystem {
                            return responder
                                .respond_with_error(acp::Error::new(-32601, "Method not found"));
                        }
                        let Some(waiter) = filesystem::prepare(
                            Arc::clone(&reads),
                            HostRequest::Read(Box::new(request)),
                        ) else {
                            return responder.respond_with_error(acp::Error::new(
                                -32000,
                                "Filesystem request rejected",
                            ));
                        };
                        cx.spawn(async move {
                            match waiter.resolve().await {
                                HostResponse::Read(content) => {
                                    responder.respond(ReadTextFileResponse::new(content))
                                }
                                _ => responder.respond_with_error(acp::Error::new(
                                    -32000,
                                    "Filesystem request rejected",
                                )),
                            }
                        })
                    },
                    acp::on_receive_request!(),
                )
                .on_receive_request(
                    async move |request: WriteTextFileRequest, responder, cx| {
                        if !writes.options.filesystem {
                            return responder
                                .respond_with_error(acp::Error::new(-32601, "Method not found"));
                        }
                        let Some(waiter) = filesystem::prepare(
                            Arc::clone(&writes),
                            HostRequest::Write(Box::new(request)),
                        ) else {
                            return responder.respond_with_error(acp::Error::new(
                                -32000,
                                "Filesystem request rejected",
                            ));
                        };
                        cx.spawn(async move {
                            match waiter.resolve().await {
                                HostResponse::Written => {
                                    responder.respond(WriteTextFileResponse::new())
                                }
                                _ => responder.respond_with_error(acp::Error::new(
                                    -32000,
                                    "Filesystem request rejected",
                                )),
                            }
                        })
                    },
                    acp::on_receive_request!(),
                );
            let foreground = Arc::clone(&worker);
            let connected =
                client.connect_with(transport, async move |connection: ConnectionTo<Agent>| {
                    let result = tokio::time::timeout(
                        foreground.options.request_timeout,
                        connection
                            .send_request(
                                InitializeRequest::new(ProtocolVersion::V1).client_capabilities(
                                    ClientCapabilities::new().fs(FileSystemCapabilities::new()
                                        .read_text_file(foreground.options.filesystem)
                                        .write_text_file(foreground.options.filesystem)),
                                ),
                            )
                            .block_task(),
                    )
                    .await;
                    let result = match result {
                        Ok(Ok(response)) if response.protocol_version == ProtocolVersion::V1 => {
                            Ok((connection, response))
                        }
                        Ok(Ok(_)) => Err(Error::Protocol("peer did not negotiate ACP v1")),
                        Ok(Err(error)) => Err(Error::Remote(error.code.value())),
                        Err(_) => {
                            eprintln!("fliuno: ACP initialize response timed out");
                            Err(Error::OutcomeUnknown)
                        }
                    };
                    if let Err(error) = &result {
                        foreground.fail(error.clone());
                    }
                    if ready.send(result).is_err() {
                        foreground.stop.cancel();
                    }
                    foreground.stop.cancelled().await;
                    Ok(())
                });
            tokio::pin!(connected);
            tokio::select! {
                _ = worker.stop.cancelled() => {},
                _ = &mut connected => {},
            }
            // Keep the protocol future alive until its process is killed, so a
            // stalled peer cannot keep a blocking pipe worker alive on shutdown.
            if let Some(peer) = peer {
                let _ = tokio::task::spawn_blocking(move || drop(peer)).await;
            }
            worker.fail(Error::Closed);
        });
        let ready = initialized.await.unwrap_or_else(|_| Err(shared.failure()));
        let (connection, initialized) = match ready {
            Ok(ready) => ready,
            Err(error) => {
                shared.stop.cancel();
                let _ = driver.await;
                return Err(error);
            }
        };
        // Disarm initialization's cancellation guard only after ownership exists.
        starting.0 = None;
        Ok((
            Self {
                inner: Arc::new(Inner {
                    connection,
                    initialized,
                    shared: Arc::clone(&shared),
                    directory,
                    catalog: tokio::sync::Mutex::new(()),
                    driver: Mutex::new(Some(driver)),
                }),
            },
            Events { receiver, shared },
        ))
    }

    pub fn initialized(&self) -> &InitializeResponse {
        &self.inner.initialized
    }

    pub fn private_directory(&self) -> Option<&Path> {
        self.inner.directory.as_deref()
    }

    fn validate_private_cwd(&self, cwd: &Path) -> Result<()> {
        if let Some(dir) = &self.inner.directory {
            let allowed = std::fs::canonicalize(dir).map_err(|_| Error::Transport)?;
            let requested =
                std::fs::canonicalize(cwd).map_err(|_| Error::InvalidInput("invalid cwd"))?;
            if !requested.starts_with(allowed) {
                return Err(Error::InvalidInput("isolated session must use private cwd"));
            }
        }
        Ok(())
    }

    pub fn is_closed(&self) -> bool {
        self.inner.shared.stop.is_cancelled()
    }

    pub fn failure(&self) -> Option<Error> {
        self.is_closed().then(|| self.inner.shared.failure())
    }

    /// One active text turn per session, at most eight concurrent turns per
    /// connection. Updates stream through Events rather than accumulating here.
    pub async fn prompt_text(
        &self,
        session_id: &SessionId,
        text: impl Into<String>,
        cancellation: &CancellationToken,
    ) -> Result<PromptResponse> {
        self.prompt_text_with_context(session_id, text, None, cancellation)
            .await
    }

    /// A new turn may include a separately bounded historical data block. Its
    /// presence never changes the current user input or grants agent authority.
    pub async fn prompt_text_with_context(
        &self,
        session_id: &SessionId,
        text: impl Into<String>,
        context: Option<String>,
        cancellation: &CancellationToken,
    ) -> Result<PromptResponse> {
        let shared = &self.inner.shared;
        shared.check()?;
        let text = text.into();
        if text.trim().is_empty() || text.len() > 256 * 1024 {
            return Err(Error::InvalidInput(
                "prompt must contain 1..256 KiB of text",
            ));
        }
        if context.as_ref().is_some_and(|text| text.len() > 257 * 1024) {
            return Err(Error::InvalidInput("historical context exceeds 257 KiB"));
        }
        let mut blocks = Vec::with_capacity(2);
        if let Some(context) = context {
            blocks.push(ContentBlock::Text(TextContent::new(context)));
        }
        blocks.push(ContentBlock::Text(TextContent::new(text)));
        let request = PromptRequest::new(session_id.clone(), blocks);
        // Reserve space for the JSON-RPC envelope. Escaped control characters
        // can make wire bytes much larger than the UTF-8 text budgets above.
        if crate::json_size::json_size(&request)
            .map_err(|_| Error::InvalidInput("invalid prompt"))?
            > 1024 * 1024 - 1024
        {
            return Err(Error::InvalidInput(
                "encoded prompt exceeds the wire budget",
            ));
        }
        if cancellation.is_cancelled() {
            return Err(Error::Cancelled);
        }
        let turn = CancellationToken::new();
        {
            let mut state = shared.state.lock().unwrap();
            if state
                .sessions
                .values()
                .filter(|s| s.active.is_some())
                .count()
                >= 8
            {
                return Err(Error::Busy);
            }
            if state.closing.as_ref() == Some(session_id) {
                return Err(Error::Busy);
            }
            let session = state.sessions.get_mut(session_id).ok_or(Error::NotFound)?;
            if session.active.is_some() {
                return Err(Error::Busy);
            }
            session.active = Some(turn.clone());
        }
        let mut guard = PromptGuard {
            shared: Arc::clone(shared),
            session_id: session_id.clone(),
            finished: false,
        };
        let response = self.inner.connection.send_request(request).block_task();
        tokio::pin!(response);
        let timed_out = tokio::select! {
            biased;
            _ = shared.stop.cancelled() => return Err(shared.failure()),
            _ = cancellation.cancelled() => false,
            _ = turn.cancelled() => false,
            result = &mut response => {
                let result = result.map_err(|error| Error::Remote(error.code.value()))?;
                guard.finished = true;
                return Ok(result);
            },
            _ = tokio::time::sleep(shared.options.prompt_timeout) => true,
        };
        turn.cancel();
        shared.cancel_permissions(session_id);
        self.inner
            .connection
            .send_notification(CancelNotification::new(session_id.clone()))
            .map_err(|_| Error::OutcomeUnknown)?;
        let final_response = tokio::select! {
            biased;
            _ = shared.stop.cancelled() => return Err(shared.failure()),
            result = tokio::time::timeout(shared.options.cancellation_grace, &mut response) => result,
        };
        if !timed_out && let Ok(Ok(result)) = final_response {
            guard.finished = true;
            return Ok(result);
        }
        Err(Error::OutcomeUnknown)
    }

    /// Request cooperative cancellation. The active prompt waits a bounded grace
    /// period, then stops the entire connection if the agent does not respond.
    pub fn cancel(&self, session_id: &SessionId) -> Result<()> {
        self.inner.shared.check()?;
        let state = self.inner.shared.state.lock().unwrap();
        let session = state.sessions.get(session_id).ok_or(Error::NotFound)?;
        if let Some(turn) = &session.active {
            turn.cancel();
        }
        Ok(())
    }

    pub fn respond_permission(
        &self,
        session_id: &SessionId,
        permission_id: &str,
        decision: PermissionDecision,
    ) -> Result<()> {
        self.inner
            .shared
            .respond_permission(session_id, permission_id, decision)
    }

    pub fn respond_host_request(
        &self,
        session_id: &SessionId,
        id: &str,
        response: HostResponse,
    ) -> Result<()> {
        self.inner
            .shared
            .respond_host_request(session_id, id, response)
    }

    /// Recheck the active turn and monotonic deadline immediately before host IO.
    pub fn host_request_remaining(
        &self,
        session_id: &SessionId,
        id: &str,
    ) -> Result<std::time::Duration> {
        self.inner.shared.host_request_remaining(session_id, id)
    }

    /// Stop the driver and wait until it has released its owned process.
    pub async fn close(&self) {
        self.inner.shared.stop.cancel();
        let driver = self.inner.driver.lock().unwrap().take();
        if let Some(driver) = driver {
            let _ = driver.await;
        } else {
            self.inner.shared.finished.cancelled().await;
        }
    }
}
