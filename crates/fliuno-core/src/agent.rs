//! Durable external-agent boundary. SDKs, processes and UI remain outside core.
use super::*;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::mpsc;

#[cfg(test)]
#[path = "agent_tests.rs"]
mod tests;

pub(crate) const MAX_RECORDED_BYTES: u64 = 8 * 1024 * 1024;
const MAX_TEXT_BYTES: usize = 1024 * 1024;
const MAX_UPDATE_BYTES: usize = 256 * 1024;
const MAX_UPDATES: u32 = 4096;
const MAX_PERMISSIONS: u32 = 64;
pub(crate) const MAX_PENDING: usize = 16;

pub struct AgentRequest {
    pub task_id: String,
    pub run_id: String,
    pub input: String,
    pub context: Option<TaskContext>,
    pub workspace: Workspace,
    pub remote_continuation: Option<RemoteContinuation>,
}

pub enum AgentCommand {
    Permission {
        permission_id: String,
        decision: ApprovalDecision,
    },
    HostTool {
        request_id: String,
        execution: Option<Box<PreparedToolExecution>>,
    },
}

pub enum AgentObservation {
    RemoteContinuationSending,
    RemoteTask(AgentRemoteTask),
    Artifacts(Vec<IncomingAgentArtifact>),
    SessionOpened(String),
    Update {
        update: serde_json::Value,
        text: Option<String>,
    },
    PermissionRequested {
        permission_id: String,
        request: serde_json::Value,
        allow_once_available: bool,
        expires_at_unix_ms: u64,
    },
    PermissionClosed {
        permission_id: String,
    },
}

pub struct AgentOutcome {
    pub stop_reason: Option<AgentStopReason>,
    pub outcome_unknown: bool,
}

impl AgentOutcome {
    pub fn unknown() -> Self {
        Self {
            stop_reason: None,
            outcome_unknown: true,
        }
    }
}

/// Observations must commit before execute continues. On error the backend must
/// close the connection, terminate its owned process and never retry the prompt.
pub trait AgentObserver: Send + Sync {
    fn observe(&self, observation: AgentObservation) -> Result<()>;
    fn observe_tool(&self, _observation: AgentToolObservation) -> Result<()> {
        Err(Error::Conflict(
            "observer does not support host tools".into(),
        ))
    }
}

pub trait AgentBackend: Send + Sync {
    fn spec(&self) -> AgentSpec;
    /// Override with the frozen process/remote registration, including resolved
    /// environment/credential policy. Only its digest enters task snapshots.
    fn authority(&self) -> String {
        authority_digest("agent.contract.v1", &serde_json::json!(self.spec()))
    }
    fn supports_remote_task_check(&self) -> bool {
        false
    }
    fn supports_remote_continuation(&self) -> bool {
        false
    }
    /// Read the current remote task using the frozen host registration. This
    /// hook may not send a prompt, cancel the task, or import content.
    fn check_remote_task(
        &self,
        _remote: &AgentRemoteTask,
        _cancellation: &CancellationToken,
    ) -> Option<AgentRemoteTask> {
        None
    }
    fn execute(
        &self,
        request: &AgentRequest,
        observer: &dyn AgentObserver,
        commands: mpsc::Receiver<AgentCommand>,
        cancellation: &CancellationToken,
    ) -> AgentOutcome;
}

pub(crate) struct AgentControl {
    run_id: String,
    sender: mpsc::Sender<AgentCommand>,
    pub cancellation: CancellationToken,
}

pub struct PreparedAgentRun {
    pub request: AgentRequest,
    backend: Arc<dyn AgentBackend>,
    commands: mpsc::Receiver<AgentCommand>,
}

impl PreparedAgentRun {
    pub fn execute(
        self,
        observer: &dyn AgentObserver,
        cancellation: &CancellationToken,
    ) -> AgentOutcome {
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.backend
                .execute(&self.request, observer, self.commands, cancellation)
        }))
        .unwrap_or_else(|_| AgentOutcome::unknown())
    }
}

pub(crate) fn close_approvals(run: &mut AgentRun) -> Vec<EventKind> {
    run.pending_approvals
        .drain(..)
        .map(|approval| EventKind::AgentApprovalClosed {
            run_id: run.id.clone(),
            approval_id: approval.id,
        })
        .collect()
}

pub(crate) fn now_ms() -> Result<u64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .map_err(|_| Error::Conflict("system clock is before UNIX epoch".into()))
}

impl Kernel {
    /// Trusted host configuration only. Each task obtains a fresh backend run.
    pub fn register_agent(&mut self, backend: Box<dyn AgentBackend>) -> Result<()> {
        let spec = backend.spec();
        if spec.id.is_empty()
            || spec.id.len() > 64
            || !spec
                .id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
            || self.agents.contains_key(&spec.id)
            || self.agents.len() >= 64
            || spec.protocol.is_empty()
            || spec.protocol.len() > 64
            || spec.isolation.is_empty()
            || spec.isolation.len() > 64
            || !authority::valid(&backend.authority())
        {
            return Err(Error::Invalid(
                "invalid or duplicate agent registration".into(),
            ));
        }
        self.agents.insert(spec.id, Arc::from(backend));
        Ok(())
    }

    pub fn agent_specs(&self) -> Vec<AgentSpec> {
        self.agents.values().map(|a| a.spec()).collect()
    }

    /// Commit the run identity before creating any process or sending a prompt.
    pub fn prepare_agent_run(
        &mut self,
        task_id: &str,
        cancellation: &CancellationToken,
    ) -> Result<Option<PreparedAgentRun>> {
        let mut task = self.get_task(task_id)?;
        if task.status != TaskStatus::Running || cancellation.is_cancelled() {
            return Ok(None);
        }
        let TaskBackend::ExternalAgent { agent_id } = &task.backend else {
            return Err(Error::Conflict("task is not an external agent task".into()));
        };
        if task.agent_run.is_some() {
            return Err(Error::Conflict("agent run already started".into()));
        }
        let Some(backend) = self.agents.get(agent_id).cloned() else {
            let message = "queued agent registration is no longer available; create a new task";
            self.fail(task, message.into())?;
            return Err(Error::Conflict(message.into()));
        };
        if task.backend_authority.as_deref() != Some(backend.authority().as_str()) {
            let message =
                "agent registration changed or lacks an authority binding; create a new task";
            // A scheduler logs preparation errors. Persist the known refusal
            // here, otherwise clients would observe an eternally running task.
            self.fail(task, message.into())?;
            return Err(Error::Conflict(message.into()));
        }
        let input = match task.messages.first() {
            Some(Message::User { text }) => text.clone(),
            _ => return Err(Error::Conflict("agent task has no input".into())),
        };
        let run_id = new_id("run");
        task.agent_run = Some(AgentRun {
            id: run_id.clone(),
            finished: false,
            session_id: None,
            outcome_unknown: true,
            stop_reason: None,
            text: String::new(),
            update_count: 0,
            recorded_bytes: 0,
            permission_count: 0,
            pending_approvals: Vec::new(),
            host_tools: Vec::new(),
            remote_task: None,
            artifacts: Vec::new(),
        });
        let workspace = self.workspace_for_task(task_id)?;
        self.commit(
            &task,
            &[EventKind::AgentStarted {
                run_id: run_id.clone(),
                agent_id: agent_id.clone(),
            }],
            &[],
        )?;
        let (sender, commands) = mpsc::channel(MAX_PENDING);
        self.agent_controls.insert(
            task.id.clone(),
            AgentControl {
                run_id: run_id.clone(),
                sender,
                cancellation: cancellation.clone(),
            },
        );
        Ok(Some(PreparedAgentRun {
            request: AgentRequest {
                task_id: task.id,
                run_id,
                input,
                context: task.context,
                workspace,
                remote_continuation: task.remote_continuation,
            },
            backend,
            commands,
        }))
    }

    pub fn observe_agent(
        &mut self,
        task_id: &str,
        run_id: &str,
        observation: AgentObservation,
    ) -> Result<()> {
        // Remote cancellation evidence may arrive after local cancellation.
        // Keep it separate from ordinary text/tool observations, which stop.
        let observation = match observation {
            AgentObservation::RemoteContinuationSending => {
                return self.mark_remote_continuation_sending(task_id, run_id);
            }
            AgentObservation::RemoteTask(remote) => {
                return self.observe_remote_task(task_id, run_id, remote);
            }
            AgentObservation::Artifacts(artifacts) => {
                return self.observe_agent_artifacts(task_id, run_id, artifacts);
            }
            observation => observation,
        };
        let mut task = self.get_task(task_id)?;
        if !matches!(
            task.status,
            TaskStatus::Running | TaskStatus::WaitingApproval
        ) {
            return Err(Error::Conflict("agent task is no longer active".into()));
        }
        let run = task
            .agent_run
            .as_mut()
            .filter(|r| r.id == run_id && !r.finished)
            .ok_or_else(|| {
                Error::Conflict("observation belongs to an inactive agent run".into())
            })?;
        let event = match observation {
            AgentObservation::RemoteContinuationSending
            | AgentObservation::RemoteTask(_)
            | AgentObservation::Artifacts(_) => unreachable!(),
            AgentObservation::SessionOpened(session_id) => {
                if run.session_id.is_some() || session_id.is_empty() || session_id.len() > 256 {
                    return Err(Error::Conflict("invalid or duplicate agent session".into()));
                }
                run.session_id = Some(session_id.clone());
                EventKind::AgentSessionOpened {
                    run_id: run_id.into(),
                    session_id,
                }
            }
            AgentObservation::Update { update, text } => {
                let bytes = serde_json::to_vec(&update)?.len();
                let text_bytes = text.as_ref().map_or(0, String::len);
                if bytes > MAX_UPDATE_BYTES
                    || run.update_count >= MAX_UPDATES
                    || run.recorded_bytes + bytes as u64 + text_bytes as u64 > MAX_RECORDED_BYTES
                    || run.text.len() + text_bytes > MAX_TEXT_BYTES
                {
                    return Err(Error::Conflict("agent update budget exceeded".into()));
                }
                run.update_count += 1;
                run.recorded_bytes += (bytes + text_bytes) as u64;
                if let Some(text) = text {
                    run.text.push_str(&text);
                }
                EventKind::AgentUpdate {
                    run_id: run_id.into(),
                    update,
                }
            }
            AgentObservation::PermissionRequested {
                permission_id,
                request,
                allow_once_available,
                expires_at_unix_ms,
            } => {
                let bytes = serde_json::to_vec(&request)?.len();
                let now = now_ms()?;
                if permission_id.is_empty()
                    || permission_id.len() > 256
                    || bytes > MAX_UPDATE_BYTES
                    || run.permission_count >= MAX_PERMISSIONS
                    || run.pending_approvals.len() >= MAX_PENDING
                    || run.recorded_bytes + bytes as u64 > MAX_RECORDED_BYTES
                    || run.session_id.is_none()
                    || expires_at_unix_ms <= now
                    || expires_at_unix_ms.saturating_sub(now) > 3_600_000
                    || run
                        .pending_approvals
                        .iter()
                        .any(|a| a.permission_id == permission_id)
                {
                    return Err(Error::Conflict(
                        "invalid permission or agent permission budget exceeded".into(),
                    ));
                }
                let approval = AgentApproval {
                    id: new_id("approval"),
                    run_id: run_id.into(),
                    permission_id,
                    source: AgentApprovalSource::AgentPermission,
                    request,
                    allow_once_available,
                    expires_at_unix_ms,
                };
                run.pending_approvals.push(approval.clone());
                run.permission_count += 1;
                run.recorded_bytes += bytes as u64;
                task.status = TaskStatus::WaitingApproval;
                EventKind::AgentApprovalRequested { approval }
            }
            AgentObservation::PermissionClosed { permission_id } => {
                let Some(index) = run.pending_approvals.iter().position(|a| {
                    a.source == AgentApprovalSource::AgentPermission
                        && a.permission_id == permission_id
                }) else {
                    return Ok(());
                };
                let approval = run.pending_approvals.remove(index);
                if run.pending_approvals.is_empty() {
                    task.status = TaskStatus::Running;
                }
                EventKind::AgentApprovalClosed {
                    run_id: run_id.into(),
                    approval_id: approval.id,
                }
            }
        };
        self.commit(&task, &[event], &[])
    }

    /// Persist the exact one-shot intent first, then nonblocking handoff. A crash
    /// between these steps leaves auditable intent and never replays authority.
    pub fn respond_agent_approval(
        &mut self,
        task_id: &str,
        approval_id: &str,
        decision: ApprovalDecision,
    ) -> Result<Task> {
        let mut task = self.get_task(task_id)?;
        if task.status != TaskStatus::WaitingApproval {
            return Err(Error::Conflict(
                "agent task is not waiting for approval".into(),
            ));
        }
        let run = task
            .agent_run
            .as_mut()
            .ok_or_else(|| Error::Conflict("task has no agent run".into()))?;
        let index = run
            .pending_approvals
            .iter()
            .position(|a| a.id == approval_id && a.run_id == run.id)
            .ok_or_else(|| {
                Error::Conflict("permission is expired or belongs to another run".into())
            })?;
        let approval = &run.pending_approvals[index];
        if approval.expires_at_unix_ms <= now_ms()? {
            return Err(Error::Conflict("agent permission expired".into()));
        }
        if decision == ApprovalDecision::AllowOnce && !approval.allow_once_available {
            return Err(Error::Conflict("agent offered no allow_once option".into()));
        }
        let control = self
            .agent_controls
            .get(task_id)
            .filter(|c| c.run_id == run.id && !c.cancellation.is_cancelled())
            .ok_or_else(|| Error::Conflict("agent connection is no longer available".into()))?;
        let sender = control.sender.clone();
        let approval = run.pending_approvals[index].clone();
        let run_id = run.id.clone();
        let command = if approval.source == AgentApprovalSource::HostTool {
            self.prepare_agent_host_command(&mut task, &approval.permission_id, decision)?
        } else {
            AgentCommand::Permission {
                permission_id: approval.permission_id.clone(),
                decision,
            }
        };
        let run = task.agent_run.as_mut().expect("checked run");
        run.pending_approvals.remove(index);
        if run.pending_approvals.is_empty() {
            task.status = TaskStatus::Running;
        }
        self.commit(
            &task,
            &[EventKind::AgentApprovalResolved {
                run_id: run_id.clone(),
                approval_id: approval.id,
                decision,
            }],
            &[],
        )?;
        if sender.try_send(command).is_err() {
            return self.finish_agent_run(task_id, &run_id, AgentOutcome::unknown());
        }
        Ok(task)
    }

    pub fn finish_agent_run(
        &mut self,
        task_id: &str,
        run_id: &str,
        outcome: AgentOutcome,
    ) -> Result<Task> {
        let mut task = self.get_task(task_id)?;
        let run = task
            .agent_run
            .as_mut()
            .filter(|r| r.id == run_id)
            .ok_or_else(|| Error::Conflict("result belongs to another agent run".into()))?;
        if run.finished {
            return Ok(task);
        }
        // Unknown runs can be interrupted early by a failed permission handoff;
        // late completion must not overwrite that conservative recovery record.
        if !matches!(
            task.status,
            TaskStatus::Running | TaskStatus::WaitingApproval | TaskStatus::Cancelled
        ) {
            return Ok(task);
        }
        let host_unknown = agent_tools::close_host_tools(run);
        let outcome_unknown = outcome.outcome_unknown || host_unknown;
        run.outcome_unknown = outcome_unknown;
        run.finished = true;
        run.stop_reason = outcome.stop_reason;
        let mut events = close_approvals(run);
        events.push(EventKind::AgentFinished {
            run_id: run_id.into(),
            stop_reason: outcome.stop_reason,
            outcome_unknown,
        });
        if task.status != TaskStatus::Cancelled {
            task.status = if outcome_unknown {
                TaskStatus::Interrupted
            } else {
                match outcome.stop_reason {
                    Some(AgentStopReason::EndTurn | AgentStopReason::Refusal) => {
                        TaskStatus::Completed
                    }
                    Some(AgentStopReason::Cancelled) => TaskStatus::Cancelled,
                    Some(
                        AgentStopReason::Failed
                        | AgentStopReason::Rejected
                        | AgentStopReason::RemotePreflightFailed,
                    ) => TaskStatus::Failed,
                    _ => TaskStatus::Interrupted,
                }
            };
            task.output = Some(run.text.clone());
            match task.status {
                TaskStatus::Completed => {
                    task.messages.push(Message::Assistant {
                        text: run.text.clone(),
                    });
                    events.push(EventKind::TaskCompleted {
                        text: run.text.clone(),
                    });
                }
                TaskStatus::Cancelled => events.push(EventKind::TaskCancelled),
                TaskStatus::Failed => events.push(EventKind::TaskFailed {
                    message: "external agent failed or rejected the task".into(),
                }),
                _ => events.push(EventKind::TaskInterrupted),
            }
        }
        self.commit(&task, &events, &[])?;
        if let Some(control) = self.agent_controls.remove(task_id) {
            control.cancellation.cancel();
        }
        Ok(task)
    }
}
