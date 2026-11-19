use fliuno_acp::{Client, ClientOptions, Event, Events, PermissionDecision, schema::*};
use fliuno_core::{
    AgentBackend, AgentCommand, AgentObservation, AgentObserver, AgentOutcome, AgentRequest,
    CancellationToken, Kernel,
};
use fliuno_protocol::{AgentSpec, AgentStopReason, ApprovalDecision};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
    time::Duration,
};
use tokio::sync::mpsc;

#[path = "acp/catalog.rs"]
mod catalog;

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AgentsConfig {
    pub agents: Vec<AcpAgentConfig>,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AcpAgentConfig {
    #[serde(default)]
    pub assets: Vec<fliuno_sandbox::StagedFile>,
    pub id: String,
    pub command: PathBuf,
    #[serde(default)]
    pub args: Vec<String>,
    pub isolation: Isolation,
    /// Fixed workspace of a trusted Agent's own state store and process.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<PathBuf>,
    /// Permit the local RPC client to launch this trusted ACP catalog reader.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub session_catalog: bool,
    /// Environment names only. Values come from the trusted host environment.
    #[serde(default)]
    pub env_allow: Vec<String>,
    /// Optional hard OS resource ceilings for this isolated Agent process.
    #[serde(default)]
    pub resources: fliuno_sandbox::ResourceLimits,
    /// Optional delegated cgroup v2 parent used by the Linux sandbox.
    #[serde(default)]
    pub linux_cgroup_parent: Option<PathBuf>,
    #[serde(default = "default_request_ms")]
    pub request_timeout_ms: u64,
    #[serde(default = "default_prompt_ms")]
    pub prompt_timeout_ms: u64,
    #[serde(default = "default_permission_ms")]
    pub permission_timeout_ms: u64,
    #[serde(default)]
    pub workspace_access: bool,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Isolation {
    NativeScratch,
    TrustedHost,
}
fn default_request_ms() -> u64 {
    30_000
}
fn default_prompt_ms() -> u64 {
    300_000
}
fn default_permission_ms() -> u64 {
    120_000
}

pub(super) fn stop_reason(reason: StopReason) -> Option<AgentStopReason> {
    match reason {
        StopReason::EndTurn => Some(AgentStopReason::EndTurn),
        StopReason::Refusal => Some(AgentStopReason::Refusal),
        StopReason::Cancelled => Some(AgentStopReason::Cancelled),
        StopReason::MaxTokens => Some(AgentStopReason::MaxTokens),
        StopReason::MaxTurnRequests => Some(AgentStopReason::MaxTurnRequests),
        _ => None,
    }
}

struct AcpBackend {
    authority: String,
    id: String,
    launch: fliuno_sandbox::LaunchSpec,
    trusted_cwd: Option<PathBuf>,
    session_catalog: bool,
    options: ClientOptions,
}

pub fn register_config(kernel: &mut Kernel, config: AgentsConfig) -> Result<(), String> {
    if config.agents.len() + kernel.agent_specs().len() > 64 {
        return Err("too many ACP registrations".into());
    }
    // Validate the whole configuration before mutating the registry.
    let mut ids: BTreeSet<String> = kernel.agent_specs().into_iter().map(|s| s.id).collect();
    let mut backends = Vec::new();
    for config in config.agents {
        if config.id.is_empty()
            || config.id.len() > 64
            || !config
                .id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
            || !ids.insert(config.id.clone())
        {
            return Err("invalid or duplicate ACP agent ID".into());
        }
        for timeout in [
            config.request_timeout_ms,
            config.prompt_timeout_ms,
            config.permission_timeout_ms,
        ] {
            if !(100..=3_600_000).contains(&timeout) {
                return Err("ACP timeout must be 100..3600000 milliseconds".into());
            }
        }
        if config.env_allow.len() > 64 {
            return Err("too many ACP environment names".into());
        }
        let mut env = BTreeMap::new();
        for name in &config.env_allow {
            if name.is_empty()
                || name.len() > 128
                || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
            {
                return Err("invalid ACP environment name".into());
            }
            match std::env::var(name) {
                Ok(value) => {
                    let key = if cfg!(windows) {
                        name.to_ascii_uppercase()
                    } else {
                        name.clone()
                    };
                    env.insert(key, value);
                }
                Err(std::env::VarError::NotPresent) => {}
                Err(_) => return Err("ACP inherited environment is not Unicode".into()),
            }
        }
        if cfg!(windows)
            && !env
                .keys()
                .any(|name| name.eq_ignore_ascii_case("SystemRoot"))
        {
            let root = std::env::var("SystemRoot")
                .map_err(|_| "ACP SystemRoot must be available and Unicode at registration")?;
            env.insert("SYSTEMROOT".into(), root);
        }
        if cfg!(windows)
            && !config
                .command
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("exe"))
        {
            return Err("ACP on Windows requires an explicit native .exe".into());
        }
        let trusted_cwd = match config.isolation {
            Isolation::NativeScratch if config.cwd.is_none() => None,
            Isolation::NativeScratch => return Err("ACP cwd requires trusted_host".into()),
            Isolation::TrustedHost => {
                if !cfg!(any(windows, target_os = "linux")) {
                    return Err("ACP trusted_host is supported on Windows and Linux".into());
                }
                if config.workspace_access || !config.assets.is_empty() {
                    return Err("ACP trusted_host does not accept workspaceAccess or assets".into());
                }
                let cwd = config
                    .cwd
                    .as_ref()
                    .filter(|path| path.is_absolute() && path.is_dir())
                    .ok_or("ACP trusted_host requires an absolute existing cwd")?;
                Some(cwd.canonicalize().map_err(|_| "cannot resolve ACP cwd")?)
            }
        };
        if config.session_catalog && trusted_cwd.is_none() {
            return Err("ACP sessionCatalog requires trusted_host".into());
        }
        let executable_digest = fliuno_sandbox::program_digest(&config.command)
            .map_err(|_| "cannot hash ACP executable")?;
        let mut identity = serde_json::json!({
            "configuration":config, "environment":env,"executableSha256":executable_digest,
        });
        if let Some(cwd) = &trusted_cwd {
            identity["resolvedCwd"] = serde_json::json!(cwd);
        }
        let authority = fliuno_core::authority_digest("acp.registration.v1", &identity);
        let launch = fliuno_sandbox::LaunchSpec {
            program_sha256: Some(executable_digest),
            assets: config.assets,
            program: config.command,
            args: config.args,
            env,
            resources: config.resources,
            linux_cgroup_parent: config.linux_cgroup_parent,
        };
        launch
            .validate()
            .map_err(|_| "invalid ACP launch configuration")?;
        backends.push(AcpBackend {
            authority,
            id: config.id,
            launch,
            trusted_cwd,
            session_catalog: config.session_catalog,
            options: ClientOptions {
                request_timeout: Duration::from_millis(config.request_timeout_ms),
                prompt_timeout: Duration::from_millis(config.prompt_timeout_ms),
                permission_timeout: Duration::from_millis(config.permission_timeout_ms),
                filesystem: config.workspace_access,
                ..ClientOptions::default()
            },
        });
    }
    for backend in backends {
        kernel
            .register_agent(Box::new(backend))
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

impl AgentBackend for AcpBackend {
    fn authority(&self) -> String {
        self.authority.clone()
    }
    fn spec(&self) -> AgentSpec {
        AgentSpec {
            id: self.id.clone(),
            protocol: "acp_v1".into(),
            isolation: if self.trusted_cwd.is_some() {
                "trusted_host"
            } else {
                "native_scratch"
            }
            .into(),
            workspace_access: self.options.filesystem,
        }
    }

    fn supports_agent_session_continuation(&self) -> bool {
        self.trusted_cwd.is_some()
    }

    fn supports_agent_session_listing(&self) -> bool {
        self.session_catalog
    }

    fn list_agent_sessions(
        &self,
        cursor: Option<&str>,
        cancellation: &CancellationToken,
    ) -> Result<fliuno_protocol::AgentSessionPage, fliuno_protocol::AgentSessionListFailure> {
        self.read_catalog(cursor, cancellation)
    }

    fn execute(
        &self,
        request: &AgentRequest,
        observer: &dyn AgentObserver,
        commands: mpsc::Receiver<AgentCommand>,
        cancellation: &CancellationToken,
    ) -> AgentOutcome {
        // Runs inside one of the bounded scheduler workers. No nested unbounded
        // agent pool and no runtime/IO while the kernel transaction is locked.
        let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        else {
            return AgentOutcome::unknown();
        };
        runtime
            .block_on(self.run(request, observer, commands, cancellation))
            .unwrap_or_else(|_| AgentOutcome::unknown())
    }
}

impl AcpBackend {
    async fn connect(&self) -> Result<(Client, Events), fliuno_acp::Error> {
        #[cfg(any(windows, target_os = "linux"))]
        if let Some(cwd) = &self.trusted_cwd {
            let program = self.launch.program.clone();
            let expected = self.launch.program_sha256.clone();
            let actual = tokio::time::timeout(
                self.options.request_timeout,
                tokio::task::spawn_blocking(move || fliuno_sandbox::program_digest(&program)),
            )
            .await
            .map_err(|_| fliuno_acp::Error::OutcomeUnknown)?
            .map_err(|_| fliuno_acp::Error::Transport)?
            .map_err(|_| fliuno_acp::Error::Transport)?;
            if Some(actual) != expected {
                return Err(fliuno_acp::Error::InvalidInput(
                    "trusted ACP executable changed",
                ));
            }
            return Client::connect_trusted_host(
                fliuno_sandbox::HostLaunchSpec {
                    program: self.launch.program.clone(),
                    args: self.launch.args.clone(),
                    env: self.launch.env.clone(),
                    cwd: cwd.clone(),
                    resources: self.launch.resources,
                    linux_cgroup_parent: self.launch.linux_cgroup_parent.clone(),
                },
                self.options.clone(),
            )
            .await;
        }
        Client::connect_isolated(self.launch.clone(), self.options.clone()).await
    }

    async fn run(
        &self,
        request: &AgentRequest,
        observer: &dyn AgentObserver,
        mut commands: mpsc::Receiver<AgentCommand>,
        cancellation: &CancellationToken,
    ) -> Result<AgentOutcome, ()> {
        let (client, mut events) = tokio::select! {
            biased;
            _ = cancellation.cancelled() => return Err(()),
            result = self.connect() => result.map_err(|error| {
                // ACP errors expose only fixed local text and numeric remote
                // codes, never peer messages, credentials or prompt content.
                eprintln!("fliuno: ACP initialization failed: {error}");
            })?,
        };
        let operations_cancel = cancellation.child_token();
        let _operations_guard = operations_cancel.clone().drop_guard();
        let result = async {
            let cwd = if let Some(cwd) = &self.trusted_cwd {
                cwd.clone()
            } else if self.options.filesystem {
                let cwd = client.private_directory().ok_or(())?.join("workspace");
                tokio::fs::create_dir(&cwd).await.map_err(|_| ())?;
                cwd
            } else { client.private_directory().ok_or(())?.to_path_buf() };
            let session = tokio::select! {
                biased;
                _ = cancellation.cancelled() => return Err(()),
                result = async {
                    if let Some(continuation) = &request.agent_session_continuation {
                        let id = SessionId::new(continuation.agent_session_id.clone());
                        client.resume_session(&id, &cwd).await?;
                        Ok(id)
                    } else {
                        client.new_session(&cwd).await.map(|session| session.session_id)
                    }
                } => match result {
                    Ok(id) => id,
                    Err(error) => {
                        eprintln!("fliuno: ACP session setup failed: {error}");
                        if request.agent_session_continuation.is_some() && matches!(error,
                            fliuno_acp::Error::Unsupported(_) | fliuno_acp::Error::Remote(_) | fliuno_acp::Error::InvalidInput(_)) {
                            return Ok(AgentOutcome { stop_reason: Some(AgentStopReason::Failed), outcome_unknown: false });
                        }
                        return Err(());
                    }
                },
            };
            let mut broker = crate::filesystem::WorkspaceBroker::new(self.options.filesystem, request.workspace.root.clone().into(), cwd, operations_cancel.clone());
            observer.observe(AgentObservation::SessionOpened(session.to_string())).map_err(|_| ())?;
            let history = fliuno_core::context_prompt(request.context.as_ref()).map_err(|_| ())?;
            let prompt = client.prompt_text_with_context(&session, request.input.clone(), history, cancellation);
            tokio::pin!(prompt);
            // Drain queued updates before accepting the final response. SDK
            // notifications can arrive in the same read as prompt completion.
            loop {
                tokio::select! {
                    biased;
                    command = commands.recv(), if !cancellation.is_cancelled() => {
                        let command = command.ok_or(())?;
                        if cancellation.is_cancelled() { continue; }
                        match command {
                            AgentCommand::Permission {permission_id, decision} => {
                                let decision = match decision {
                                    ApprovalDecision::AllowOnce => PermissionDecision::AllowOnce,
                                    ApprovalDecision::Deny => PermissionDecision::Deny,
                                };
                                client.respond_permission(&session, &permission_id, decision).map_err(|_| ())?;
                            }
                            AgentCommand::HostTool {request_id, execution} => broker.command(request_id, execution, observer, &client, &session)?,
                        }
                    }
                    job = broker.jobs.join_next(), if !broker.jobs.is_empty() => {
                        broker.completed(job.ok_or(())?.map_err(|_| ())?, observer, &client, &session)?;
                    }
                    event = events.recv() => match event.ok_or(())? {
                        Event::HostRequest {id, request, expires_at_unix_ms} => {
                            if request.session_id() != &session { return Err(()); }
                            let request_id = id.clone();
                            match broker.request(id, request, expires_at_unix_ms, observer) {
                                Ok(()) => {},
                                Err(crate::filesystem::RequestError::Rejected) => client.respond_host_request(&session, &request_id, fliuno_acp::HostResponse::Failed).map_err(|_| ())?,
                                Err(crate::filesystem::RequestError::Fatal) => return Err(()),
                            }
                        }
                        Event::HostRequestClosed {id, session_id} => {
                            if session_id != session { return Err(()); }
                            broker.closed(id, observer)?;
                        }
                        // This runner accepts live updates only. Resume does not replay history.
                        Event::SessionHistory(_) => return Err(()),
                        Event::Update(update) => {
                            if update.session_id != session { return Err(()); }
                            let text = match &update.update {
                                SessionUpdate::AgentMessageChunk(chunk) => match &chunk.content {
                                    ContentBlock::Text(text) => Some(text.text.clone()),
                                    _ => None,
                                },
                                _ => None,
                            };
                            observer.observe(AgentObservation::Update {update: serde_json::to_value(&update).map_err(|_| ())?, text}).map_err(|_| ())?;
                        }
                        Event::PermissionRequested {id, request, expires_at_unix_ms} => {
                            if request.session_id != session { return Err(()); }
                            observer.observe(AgentObservation::PermissionRequested {
                                permission_id: id, allow_once_available: request.options.iter().any(|o| o.kind == PermissionOptionKind::AllowOnce),
                                request: serde_json::to_value(&request).map_err(|_| ())?, expires_at_unix_ms,
                            }).map_err(|_| ())?;
                        }
                        Event::PermissionClosed {id, session_id, ..} => {
                            if session_id != session { return Err(()); }
                            observer.observe(AgentObservation::PermissionClosed {permission_id: id}).map_err(|_| ())?;
                        }
                    },
                    response = &mut prompt => {
                        let reason = stop_reason(response.map_err(|_| ())?.stop_reason).ok_or(())?;
                        return Ok(AgentOutcome {stop_reason: Some(reason), outcome_unknown: false});
                    }
                }
            }
        }.await;
        operations_cancel.cancel();
        // Always wait for Job/profile cleanup, including observer and wire errors.
        client.close().await;
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn acp_stop_reasons_preserve_terminal_and_incomplete_outcomes() {
        assert_eq!(
            stop_reason(StopReason::EndTurn),
            Some(AgentStopReason::EndTurn)
        );
        assert_eq!(
            stop_reason(StopReason::Refusal),
            Some(AgentStopReason::Refusal)
        );
        assert_eq!(
            stop_reason(StopReason::Cancelled),
            Some(AgentStopReason::Cancelled)
        );
        assert_eq!(
            stop_reason(StopReason::MaxTokens),
            Some(AgentStopReason::MaxTokens)
        );
        assert_eq!(
            stop_reason(StopReason::MaxTurnRequests),
            Some(AgentStopReason::MaxTurnRequests)
        );
    }
}
