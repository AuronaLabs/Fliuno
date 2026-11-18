//! Maps bounded remote A2A tasks to the neutral core. URLs, credentials and
//! deadlines are host-owned; cards/messages never confer local tool authority.
use fliuno_a2a::{
    Client, Message, MessageTarget, Options, SendResponse, StreamEvent, Task, TaskState,
};
use fliuno_core::{
    AgentBackend, AgentCommand, AgentObservation, AgentObserver, AgentOutcome, AgentRequest,
    CancellationToken, IncomingAgentArtifact, Kernel,
};
use fliuno_protocol::{
    AgentRemoteTask, AgentSpec, AgentStopReason, AgentTaskState, RemoteContinuation,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    time::Duration,
};
use tokio::{
    sync::mpsc,
    time::{Instant, sleep, sleep_until, timeout},
};

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct A2aConfig {
    pub agents: Vec<A2aAgentConfig>,
}
#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct A2aAgentConfig {
    pub id: String,
    pub endpoint: String,
    /// Optional same-origin discovery URL. Its interface must match endpoint.
    #[serde(default)]
    pub agent_card_url: Option<String>,
    #[serde(default)]
    pub bearer_token_env: Option<String>,
    #[serde(default = "request_ms")]
    pub request_timeout_ms: u64,
    #[serde(default = "task_ms")]
    pub task_timeout_ms: u64,
    #[serde(default = "poll_ms")]
    pub poll_interval_ms: u64,
    #[serde(default = "response_bytes")]
    pub max_response_bytes: usize,
    #[serde(default, skip_serializing_if = "is_false")]
    pub streaming: bool,
}
fn is_false(value: &bool) -> bool {
    !value
}
fn request_ms() -> u64 {
    30_000
}
fn task_ms() -> u64 {
    300_000
}
fn poll_ms() -> u64 {
    500
}
fn response_bytes() -> usize {
    1024 * 1024
}

struct A2aBackend {
    authority: String,
    id: String,
    options: Options,
    card: Option<String>,
    task_timeout: Duration,
    poll_interval: Duration,
    streaming: bool,
}
pub fn register_a2a_config(kernel: &mut Kernel, config: A2aConfig) -> Result<(), String> {
    let mut ids: BTreeSet<_> = kernel.agent_specs().into_iter().map(|s| s.id).collect();
    if config.agents.len() + ids.len() > 64 {
        return Err("too many A2A registrations".into());
    }
    let mut backends = Vec::new();
    for config in config.agents {
        if config.id.is_empty()
            || config.id.len() > 64
            || !config
                .id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
            || !ids.insert(config.id.clone())
            || !(100..=3_600_000).contains(&config.task_timeout_ms)
            || !(25..=30_000).contains(&config.poll_interval_ms)
        {
            return Err("invalid A2A identity or timeout configuration".into());
        }
        let endpoint =
            fliuno_a2a::validate_endpoint(&config.endpoint).map_err(|e| e.to_string())?;
        if let Some(card) = &config.agent_card_url {
            let card = fliuno_a2a::validate_endpoint(card).map_err(|e| e.to_string())?;
            if endpoint.origin() != card.origin() {
                return Err("A2A card must use the configured endpoint origin".into());
            }
        }
        let bearer_token = config
            .bearer_token_env
            .as_ref()
            .map(|name| {
                if name.is_empty()
                    || name.len() > 128
                    || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
                {
                    return Err("invalid A2A credential environment name".to_owned());
                }
                std::env::var(name)
                    .map_err(|_| "A2A credential environment variable is unavailable".to_owned())
            })
            .transpose()?;
        let authority = fliuno_core::authority_digest(
            "a2a.registration.v1",
            &json!({
                "configuration":config,"credential":bearer_token,
            }),
        );
        let options = Options {
            endpoint: config.endpoint,
            bearer_token,
            request_timeout: Duration::from_millis(config.request_timeout_ms),
            max_response_bytes: config.max_response_bytes,
        };
        Client::new(options.clone()).map_err(|e| e.to_string())?;
        backends.push(A2aBackend {
            authority,
            id: config.id,
            options,
            card: config.agent_card_url,
            task_timeout: Duration::from_millis(config.task_timeout_ms),
            poll_interval: Duration::from_millis(config.poll_interval_ms),
            streaming: config.streaming,
        });
    }
    // Validate the whole file before mutating the shared ACP/A2A registry.
    for backend in backends {
        kernel
            .register_agent(Box::new(backend))
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}
impl AgentBackend for A2aBackend {
    fn authority(&self) -> String {
        self.authority.clone()
    }
    fn spec(&self) -> AgentSpec {
        AgentSpec {
            id: self.id.clone(),
            protocol: "a2a_v1_jsonrpc".into(),
            isolation: "remote".into(),
            workspace_access: false,
        }
    }
    fn supports_remote_task_check(&self) -> bool {
        true
    }
    fn supports_remote_continuation(&self) -> bool {
        true
    }
    fn check_remote_task(
        &self,
        expected: &AgentRemoteTask,
        cancellation: &CancellationToken,
    ) -> Option<AgentRemoteTask> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .ok()?;
        runtime.block_on(async {
            let client = Client::new(self.options.clone()).ok()?;
            let budget = self.task_timeout.min(Duration::from_secs(5));
            let task = tokio::select! {
                biased;
                _ = cancellation.cancelled() => return None,
                result = timeout(budget, async {
                    if let Some(card) = &self.card {
                        client.check_card(card).await?;
                    }
                    client.get_task(&expected.id).await
                }) => result.ok()?.ok()?,
            };
            remote_task(&task).ok()
        })
    }
    fn execute(
        &self,
        request: &AgentRequest,
        observer: &dyn AgentObserver,
        _commands: mpsc::Receiver<AgentCommand>,
        cancellation: &CancellationToken,
    ) -> AgentOutcome {
        let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        else {
            return before_submission_failure(request.remote_continuation.as_ref());
        };
        runtime
            .block_on(self.run(request, observer, cancellation))
            .unwrap_or_else(|_| AgentOutcome::unknown())
    }
}
fn known(reason: AgentStopReason) -> AgentOutcome {
    AgentOutcome {
        stop_reason: Some(reason),
        outcome_unknown: false,
    }
}
fn before_submission_failure(continuation: Option<&RemoteContinuation>) -> AgentOutcome {
    if continuation.is_some() {
        known(AgentStopReason::RemotePreflightFailed)
    } else {
        AgentOutcome::unknown()
    }
}

fn verify_initial_task(continuation: Option<&RemoteContinuation>, task: &Task) -> Result<(), ()> {
    if let Some(continuation) = continuation {
        let expected = &continuation.expected;
        let same_task = task.id == expected.id;
        if task.context_id != expected.context_id
            || same_task != (expected.state == AgentTaskState::InputRequired)
        {
            return Err(());
        }
    }
    Ok(())
}

fn verify_initial_message(
    continuation: Option<&RemoteContinuation>,
    message: &Message,
) -> Result<(), ()> {
    if let Some(continuation) = continuation
        && (continuation.expected.state != AgentTaskState::Completed
            || message.context_id != continuation.expected.context_id
            || message.task_id.is_some())
    {
        return Err(());
    }
    Ok(())
}
fn stop_reason(state: TaskState) -> Option<AgentStopReason> {
    match state {
        TaskState::Completed => Some(AgentStopReason::EndTurn),
        TaskState::Failed => Some(AgentStopReason::Failed),
        TaskState::Canceled => Some(AgentStopReason::Cancelled),
        TaskState::Rejected => Some(AgentStopReason::Rejected),
        TaskState::InputRequired => Some(AgentStopReason::InputRequired),
        TaskState::AuthRequired => Some(AgentStopReason::AuthRequired),
        _ => None,
    }
}
fn remote_task(task: &Task) -> Result<AgentRemoteTask, ()> {
    let state = match task.state {
        TaskState::Submitted => AgentTaskState::Submitted,
        TaskState::Working => AgentTaskState::Working,
        TaskState::Completed => AgentTaskState::Completed,
        TaskState::Failed => AgentTaskState::Failed,
        TaskState::Canceled => AgentTaskState::Cancelled,
        TaskState::Rejected => AgentTaskState::Rejected,
        TaskState::InputRequired => AgentTaskState::InputRequired,
        TaskState::AuthRequired => AgentTaskState::AuthRequired,
        TaskState::Unspecified => return Err(()),
    };
    Ok(AgentRemoteTask {
        id: task.id.clone(),
        context_id: task.context_id.clone(),
        state,
    })
}
fn record_task(observer: &dyn AgentObserver, task: &Task) -> Result<(), ()> {
    observer
        .observe(AgentObservation::RemoteTask(remote_task(task)?))
        .map_err(|_| ())
}
fn record_message(observer: &dyn AgentObserver, message: Message) -> Result<AgentOutcome, ()> {
    if let Some(context) = &message.context_id {
        observer
            .observe(AgentObservation::SessionOpened(context.clone()))
            .map_err(|_| ())?;
    }
    observer
        .observe(AgentObservation::Artifacts(vec![artifact(
            message.id.clone(),
            "A2A message".into(),
            &message.raw,
        )?]))
        .map_err(|_| ())?;
    observer
        .observe(AgentObservation::Update {
            update: json!({"type":"a2a_message","messageId":message.id}),
            text: Some(message.text()),
        })
        .map_err(|_| ())?;
    Ok(known(AgentStopReason::EndTurn))
}

#[derive(Clone)]
struct StreamArtifact {
    raw: Value,
    complete: bool,
}

fn add_stream_artifact(
    artifacts: &mut BTreeMap<String, StreamArtifact>,
    artifact: Value,
    append: bool,
    last_chunk: Option<bool>,
) -> Result<(), ()> {
    let id = artifact["artifactId"].as_str().ok_or(())?.to_owned();
    let complete = if append {
        last_chunk.unwrap_or(false)
    } else {
        last_chunk.unwrap_or(true)
    };
    if append {
        let current = artifacts.get_mut(&id).ok_or(())?;
        if current.complete {
            return Err(());
        }
        let new_parts = artifact["parts"].as_array().ok_or(())?;
        current
            .raw
            .get_mut("parts")
            .and_then(Value::as_array_mut)
            .ok_or(())?
            .extend(new_parts.iter().cloned());
        for key in ["name", "description", "metadata"] {
            if let Some(value) = artifact.get(key) {
                current.raw[key] = value.clone();
            }
        }
        current.complete = complete;
    } else {
        if !artifacts.contains_key(&id) && artifacts.len() >= 64 {
            return Err(());
        }
        artifacts.insert(
            id.clone(),
            StreamArtifact {
                raw: artifact,
                complete,
            },
        );
    }
    let current = artifacts.get(&id).ok_or(())?;
    if serde_json::to_vec(&current.raw).map_err(|_| ())?.len() > 262_144 {
        return Err(());
    }
    Task::parse(json!({
        "id":"stream-validation",
        "status":{"state":"TASK_STATE_WORKING"},
        "artifacts":[current.raw],
    }))
    .map_err(|_| ())?;
    Ok(())
}

fn finish_stream_snapshot(
    observer: &dyn AgentObserver,
    mut task: Task,
    artifacts: BTreeMap<String, StreamArtifact>,
    status_event_recorded: bool,
    status_text_recorded: bool,
) -> Result<AgentOutcome, ()> {
    if artifacts.values().any(|artifact| !artifact.complete) {
        return Err(());
    }
    task.artifacts = artifacts
        .into_values()
        .map(|artifact| artifact.raw)
        .collect();
    if status_event_recorded {
        task.status_message = None;
    }
    finish_snapshot_with_status_text(observer, task, status_text_recorded)
}

fn artifact(source_id: String, title: String, raw: &Value) -> Result<IncomingAgentArtifact, ()> {
    Ok(IncomingAgentArtifact {
        source_id,
        title,
        media_type: "application/json".into(),
        content: serde_json::to_string(raw).map_err(|_| ())?,
    })
}
fn finish_snapshot(observer: &dyn AgentObserver, task: Task) -> Result<AgentOutcome, ()> {
    finish_snapshot_with_status_text(observer, task, false)
}

fn finish_snapshot_with_status_text(
    observer: &dyn AgentObserver,
    task: Task,
    mut has_status_text: bool,
) -> Result<AgentOutcome, ()> {
    let reason = stop_reason(task.state).ok_or(())?;
    if let Some(message) = task.status_message {
        let text = message.text();
        has_status_text |= !text.is_empty();
        observer
            .observe(AgentObservation::Update {
                update: json!({"type":"a2a_status_message","message":message.raw}),
                text: Some(text),
            })
            .map_err(|_| ())?;
    }
    // Paused tasks remain remote. We record why local execution stopped, but do
    // not claim that a partial artifact is final or resume with fresh authority.
    if task.state.is_terminal() && !task.artifacts.is_empty() {
        let mut incoming = Vec::new();
        let mut text = Vec::new();
        for raw in task.artifacts {
            let source_id = raw["artifactId"].as_str().ok_or(())?.to_owned();
            let title = raw
                .get("name")
                .and_then(Value::as_str)
                .filter(|s| !s.trim().is_empty())
                .unwrap_or(&source_id)
                .to_owned();
            text.extend(
                raw["parts"]
                    .as_array()
                    .ok_or(())?
                    .iter()
                    .filter_map(|p| p["text"].as_str())
                    .map(str::to_owned),
            );
            incoming.push(artifact(source_id, title, &raw)?);
        }
        observer
            .observe(AgentObservation::Artifacts(incoming))
            .map_err(|_| ())?;
        let mut text = text.join("\n");
        if has_status_text && !text.is_empty() {
            text.insert(0, '\n');
        }
        observer
            .observe(AgentObservation::Update {
                update: json!({"type":"a2a_artifacts_imported"}),
                text: Some(text),
            })
            .map_err(|_| ())?;
    }
    Ok(known(reason))
}
impl A2aBackend {
    async fn run(
        &self,
        request: &AgentRequest,
        observer: &dyn AgentObserver,
        cancellation: &CancellationToken,
    ) -> Result<AgentOutcome, ()> {
        // Each worker owns its runtime and connection pool. Reusing a client
        // after dropping that runtime could leave pooled sockets on a dead IO
        // reactor and turn the next one-shot submission into a transport loss.
        let continuation = request.remote_continuation.as_ref();
        let Ok(client) = Client::new(self.options.clone()) else {
            return Ok(before_submission_failure(continuation));
        };
        let deadline = Instant::now() + self.task_timeout;
        let history = fliuno_core::context_prompt(request.context.as_ref()).map_err(|_| ())?;
        if self.card.is_some() || continuation.is_some() {
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => return Ok(known(AgentStopReason::Cancelled)),
                _ = sleep_until(deadline) => return Ok(before_submission_failure(continuation)),
                result = async {
                    if let Some(card) = &self.card {
                        if self.streaming {
                            client.check_card_streaming(card).await.map_err(|_| ())?;
                        } else {
                            client.check_card(card).await.map_err(|_| ())?;
                        }
                    }
                    if let Some(continuation) = continuation {
                        let current = client.get_task(&continuation.expected.id).await.map_err(|_| ())?;
                        if remote_task(&current)? != continuation.expected {
                            return Err(());
                        }
                    }
                    Ok(())
                } => if result.is_err() { return Ok(before_submission_failure(continuation)); },
            }
        }
        let target = if let Some(continuation) = continuation {
            let expected = &continuation.expected;
            let Some(context_id) = expected.context_id.as_deref() else {
                return Ok(before_submission_failure(
                    request.remote_continuation.as_ref(),
                ));
            };
            Some(MessageTarget {
                context_id,
                task_id: (expected.state == AgentTaskState::InputRequired)
                    .then_some(expected.id.as_str()),
            })
        } else {
            None
        };
        let Ok(submission) = client.prepare_text(
            &request.run_id,
            &request.input,
            history.as_deref(),
            target,
            self.streaming,
        ) else {
            return Ok(before_submission_failure(continuation));
        };
        if cancellation.is_cancelled() {
            return Ok(known(AgentStopReason::Cancelled));
        }
        if continuation.is_some()
            && (Instant::now() >= deadline
                || observer
                    .observe(AgentObservation::RemoteContinuationSending)
                    .is_err())
        {
            return Ok(before_submission_failure(continuation));
        }
        if self.streaming {
            let events = tokio::select! {
                biased;
                _ = cancellation.cancelled() => return Err(()),
                _ = sleep_until(deadline) => return Err(()),
                result = submission.send_streaming() => result.map_err(|_| ())?,
            };
            return self
                .run_stream(
                    &client,
                    events,
                    observer,
                    cancellation,
                    deadline,
                    continuation,
                )
                .await;
        }
        // Persisted run ID doubles as a message identity for audit/correlation;
        // neither a timeout nor the same ID grants permission to resend it.
        let response = tokio::select! {
            biased;
            _ = cancellation.cancelled() => return Err(()),
            _ = sleep_until(deadline) => return Err(()),
            result = submission.send() => result.map_err(|error| {
                eprintln!("fliuno: A2A submission failed: {error}");
            })?,
        };
        let mut task = match response {
            SendResponse::Message(message) => {
                verify_initial_message(continuation, &message)?;
                return record_message(observer, message);
            }
            SendResponse::Task(task) => task,
        };
        verify_initial_task(continuation, &task)?;
        record_task(observer, &task)?;
        let mut polls = 0;
        loop {
            // Remote identity is its own durable record, not an ACP session.
            // Cancellation can race the first response: keep the identity, then
            // settle the remote task even if ordinary observations are closed.
            if cancellation.is_cancelled() {
                if task.state.is_terminal() {
                    return Ok(known(stop_reason(task.state).ok_or(())?));
                }
                return self.cancel_remote(&client, observer, &task, true).await;
            }
            if !task.state.is_running() {
                return finish_snapshot(observer, task);
            }
            if polls >= 4096 {
                return self
                    .cancel_remote(&client, observer, &task, cancellation.is_cancelled())
                    .await;
            }
            let cancel = tokio::select! {
                biased;
                _ = cancellation.cancelled() => true,
                _ = sleep_until(deadline) => true,
                _ = sleep(self.poll_interval) => false,
            };
            if cancel || polls >= 4096 {
                return self
                    .cancel_remote(&client, observer, &task, cancellation.is_cancelled())
                    .await;
            }
            let next = tokio::select! {
                biased;
                _ = cancellation.cancelled() => return self.cancel_remote(&client, observer, &task, true).await,
                _ = sleep_until(deadline) => return self.cancel_remote(&client, observer, &task, false).await,
                result = client.get_task(&task.id) => result.map_err(|_| ())?,
            };
            if !next.matches(&task) {
                return Err(());
            }
            if !task.state.allows_transition_to(next.state) {
                return Err(());
            }
            record_task(observer, &next)?;
            task = next;
            polls += 1;
        }
    }

    async fn run_stream(
        &self,
        client: &Client,
        mut events: mpsc::Receiver<fliuno_a2a::Result<StreamEvent>>,
        observer: &dyn AgentObserver,
        cancellation: &CancellationToken,
        deadline: Instant,
        continuation: Option<&RemoteContinuation>,
    ) -> Result<AgentOutcome, ()> {
        let mut task: Option<Task> = None;
        let mut artifacts = BTreeMap::new();
        let mut status_event_recorded = false;
        let mut status_text_recorded = false;
        loop {
            let event = tokio::select! {
                biased;
                _ = cancellation.cancelled() => {
                    let Some(current) = task.as_ref() else { return Err(()) };
                    if current.state.is_terminal() {
                        return finish_stream_snapshot(
                            observer,
                            current.clone(),
                            artifacts,
                            status_event_recorded,
                            status_text_recorded,
                        );
                    }
                    return self.cancel_remote(client, observer, current, true).await;
                }
                _ = sleep_until(deadline) => {
                    let Some(current) = task.as_ref() else { return Err(()) };
                    return self.cancel_remote(client, observer, current, false).await;
                }
                event = events.recv() => event,
            };
            let Some(event) = event else { return Err(()) };
            match event.map_err(|_| ())? {
                StreamEvent::Task(initial) => {
                    if task.is_some() {
                        return Err(());
                    }
                    verify_initial_task(continuation, &initial)?;
                    let settled = initial.state.is_settled();
                    for raw in &initial.artifacts {
                        let id = raw["artifactId"].as_str().ok_or(())?.to_owned();
                        artifacts.insert(
                            id,
                            StreamArtifact {
                                raw: raw.clone(),
                                complete: true,
                            },
                        );
                    }
                    record_task(observer, &initial)?;
                    if let Some(message) = &initial.status_message {
                        let text = message.text();
                        status_text_recorded |= !text.is_empty();
                        observer
                            .observe(AgentObservation::Update {
                                update: json!({"type":"a2a_status_message","message":message.raw}),
                                text: Some(text),
                            })
                            .map_err(|_| ())?;
                        status_event_recorded = true;
                    }
                    task = Some(initial);
                    if settled {
                        return finish_stream_snapshot(
                            observer,
                            task.take().ok_or(())?,
                            artifacts,
                            status_event_recorded,
                            status_text_recorded,
                        );
                    }
                }
                StreamEvent::Message(message) => {
                    if task.is_some() {
                        return Err(());
                    }
                    verify_initial_message(continuation, &message)?;
                    return record_message(observer, message);
                }
                StreamEvent::StatusUpdate {
                    task_id,
                    context_id,
                    state,
                    message,
                } => {
                    let current = task.as_mut().ok_or(())?;
                    if current.id != task_id
                        || current.context_id != context_id
                        || !current.state.allows_transition_to(state)
                    {
                        return Err(());
                    }
                    current.state = state;
                    current.status_message = message.clone();
                    record_task(observer, current)?;
                    let text = message
                        .as_ref()
                        .map(Message::text)
                        .filter(|text| !text.is_empty());
                    status_text_recorded |= text.is_some();
                    observer
                        .observe(AgentObservation::Update {
                            update: json!({
                                "type":"a2a_status_update",
                                "state":state,
                                "message":message.as_ref().map(|message| &message.raw),
                            }),
                            text,
                        })
                        .map_err(|_| ())?;
                    status_event_recorded = true;
                    if state.is_settled() {
                        return finish_stream_snapshot(
                            observer,
                            task.take().ok_or(())?,
                            std::mem::take(&mut artifacts),
                            status_event_recorded,
                            status_text_recorded,
                        );
                    }
                }
                StreamEvent::ArtifactUpdate {
                    task_id,
                    context_id,
                    artifact,
                    append,
                    last_chunk,
                } => {
                    let current = task.as_ref().ok_or(())?;
                    if current.id != task_id || current.context_id != context_id {
                        return Err(());
                    }
                    let artifact_id = artifact["artifactId"].as_str().ok_or(())?.to_owned();
                    add_stream_artifact(&mut artifacts, artifact, append, last_chunk)?;
                    observer
                        .observe(AgentObservation::Update {
                            update: json!({
                                "type":"a2a_artifact_update",
                                "artifactId":artifact_id,
                                "append":append,
                                "lastChunk":last_chunk,
                            }),
                            text: None,
                        })
                        .map_err(|_| ())?;
                }
            }
        }
    }

    async fn cancel_remote(
        &self,
        client: &Client,
        observer: &dyn AgentObserver,
        previous: &Task,
        local_cancelled: bool,
    ) -> Result<AgentOutcome, ()> {
        // Independent, strictly bounded cleanup: the task cancellation token is
        // already signaled. A cancel request alone is never proof of stopping.
        let task = timeout(Duration::from_secs(2), async {
            match client.cancel_task(&previous.id).await {
                // TaskNotCancelable can race completion. Only a matching fresh
                // read corroborates the result; no second mutation is attempted.
                Err(fliuno_a2a::Error::Remote(-32002)) => client.get_task(&previous.id).await,
                result => result,
            }
        })
        .await
        .map_err(|_| ())?
        .map_err(|_| ())?;
        if !task.matches(previous) {
            return Err(());
        }
        if !previous.state.allows_transition_to(task.state) {
            return Err(());
        }
        record_task(observer, &task)?;
        if !task.state.is_terminal() {
            return Err(());
        }
        if local_cancelled {
            return Ok(known(stop_reason(task.state).ok_or(())?));
        }
        finish_snapshot(observer, task)
    }
}
