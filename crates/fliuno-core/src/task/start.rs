//! Ordinary task submission and durable request identity.
use super::{validate_input, validate_key};
use crate::*;
use sha2::{Digest, Sha256};

pub enum TaskSummaryStart {
    Created(Box<Task>),
    Existing(TaskSummary),
}

pub(crate) enum StartLookup {
    Missing,
    Legacy,
    Existing(TaskSummary),
}

pub(crate) struct StartRequest<'a> {
    pub session: &'a str,
    pub input: StartInput<'a>,
    pub key: Option<&'a str>,
    pub backend: TaskBackend,
    pub options: ContextOptions,
    pub max_model_steps: Option<u32>,
    explicit_context: bool,
}

#[derive(Clone, Copy)]
enum Prompt<'a> {
    Text(&'a str),
    Parts(&'a [UserContentPart]),
}

impl<'a> From<&'a TaskInput> for Prompt<'a> {
    fn from(input: &'a TaskInput) -> Self {
        match input {
            TaskInput::Text(text) => Self::Text(text),
            TaskInput::Parts(parts) => Self::Parts(parts),
        }
    }
}

impl Prompt<'_> {
    pub(crate) fn digest(self) -> [u8; 32] {
        match self {
            Self::Text(text) => Sha256::digest(text.as_bytes()).into(),
            Self::Parts(parts) => {
                let mut digest = Sha256::new();
                // An invalid UTF-8 prefix separates parts from legacy text receipts.
                digest.update(b"\xfffliuno-parts-v1");
                for part in parts {
                    let (kind, data) = match part {
                        UserContentPart::Text { text } => ("text", text),
                        UserContentPart::Image { mime_type, data } => (mime_type.as_str(), data),
                        UserContentPart::Pdf { data } => (PDF_MIME_TYPE, data),
                    };
                    for field in [kind, data.as_str()] {
                        digest.update((field.len() as u64).to_be_bytes());
                        digest.update(field.as_bytes());
                    }
                }
                digest.finalize().into()
            }
        }
    }

    fn validate_media(self) -> Result<()> {
        if let Self::Parts(parts) = self {
            validate_inline_media(parts).map_err(|message| Error::Invalid(message.into()))?;
        }
        Ok(())
    }

    fn matches(self, message: Option<&Message>) -> bool {
        match (self, message) {
            (Self::Text(expected), Some(Message::User { text })) => expected == text,
            (Self::Parts(expected), Some(Message::UserParts { parts })) => expected == parts,
            _ => false,
        }
    }

    fn message(self) -> Message {
        match self {
            Self::Text(text) => Message::User { text: text.into() },
            Self::Parts(parts) => Message::UserParts {
                parts: parts.to_vec(),
            },
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) struct StartInput<'a> {
    prompt: Prompt<'a>,
    artifacts: &'a [ArtifactGetParams],
}

impl StartInput<'_> {
    pub(crate) fn digest(self) -> [u8; 32] {
        let prompt = self.prompt.digest();
        if self.artifacts.is_empty() {
            return prompt;
        }
        let mut digest = Sha256::new();
        digest.update(b"\xfffliuno-artifact-inputs-v1");
        digest.update(prompt);
        for reference in self.artifacts {
            for value in [&reference.task_id, &reference.artifact_id] {
                digest.update((value.len() as u64).to_be_bytes());
                digest.update(value.as_bytes());
            }
        }
        digest.finalize().into()
    }

    fn validate_limits(self, backend: &TaskBackend) -> Result<()> {
        if *backend != TaskBackend::Model
            && (!self.artifacts.is_empty() || matches!(self.prompt, Prompt::Parts(_)))
        {
            return Err(Error::Invalid(
                "multipart input applies only to model tasks".into(),
            ));
        }
        if self.artifacts.len() > MAX_TASK_ARTIFACT_INPUTS
            || self.artifacts.iter().any(|reference| {
                [&reference.task_id, &reference.artifact_id]
                    .iter()
                    .any(|id| {
                        id.is_empty() || id.len() > 128 || !id.bytes().all(|b| b.is_ascii_graphic())
                    })
            })
        {
            return Err(Error::Invalid(
                "artifact inputs need at most four pairs of valid identifiers".into(),
            ));
        }
        match self.prompt {
            Prompt::Text(text) if self.artifacts.is_empty() || !text.is_empty() => {
                validate_input(text)
            }
            Prompt::Text(_) => Ok(()),
            Prompt::Parts(parts) if self.artifacts.is_empty() => {
                validate_multipart_input_limits(parts)
                    .map_err(|message| Error::Invalid(message.into()))
            }
            Prompt::Parts(parts) => TaskInputBudget::with_parts(parts, self.artifacts.len())
                .map(|_| ())
                .map_err(|message| Error::Invalid(message.into())),
        }
    }

    fn message(self, kernel: &Kernel, session: &str) -> Result<Message> {
        if self.artifacts.is_empty() {
            return Ok(self.prompt.message());
        }
        let mut parts = match self.prompt {
            Prompt::Text("") => Vec::new(),
            Prompt::Text(text) => vec![UserContentPart::Text { text: text.into() }],
            Prompt::Parts(parts) => parts.to_vec(),
        };
        let mut budget = TaskInputBudget::with_parts(&parts, self.artifacts.len())
            .map_err(|message| Error::Invalid(message.into()))?;
        for reference in self.artifacts {
            parts.push(kernel.read_artifact_input(session, reference, &mut budget)?);
        }
        if parts
            .iter()
            .all(|part| matches!(part, UserContentPart::Text { text } if text.trim().is_empty()))
        {
            return Err(Error::Invalid("input text must not be empty".into()));
        }
        Ok(Message::UserParts { parts })
    }
}

pub(crate) fn request_conflict() -> Error {
    Error::Conflict(
        "clientRequestId was used for different input, backend, context or model step budget"
            .into(),
    )
}

impl Kernel {
    pub fn start_task(&mut self, session_id: &str, input: &str) -> Result<Task> {
        self.start_task_with_key(session_id, input, None)
    }

    /// The key, task and initial event are inserted in the same transaction.
    /// A duplicate key returns the existing task and never drives it implicitly.
    pub fn start_task_with_key(
        &mut self,
        session_id: &str,
        input: &str,
        key: Option<&str>,
    ) -> Result<Task> {
        let (task, created) = self.create_task_with_key(session_id, input, key)?;
        if created { self.drive(task) } else { Ok(task) }
    }

    /// Persist a task without invoking the model. The host schedules a new task.
    pub fn create_task_with_key(
        &mut self,
        session_id: &str,
        input: &str,
        key: Option<&str>,
    ) -> Result<(Task, bool)> {
        self.create_task_with_backend(session_id, input, key, TaskBackend::Model)
    }

    pub fn create_task_with_backend(
        &mut self,
        session_id: &str,
        input: &str,
        key: Option<&str>,
        backend: TaskBackend,
    ) -> Result<(Task, bool)> {
        self.create_task_with_context(session_id, input, key, backend, None)
    }

    pub fn create_task_with_context(
        &mut self,
        session_id: &str,
        input: &str,
        key: Option<&str>,
        backend: TaskBackend,
        context_options: Option<ContextOptions>,
    ) -> Result<(Task, bool)> {
        let request = self.prepare_task_start(
            session_id,
            StartInput {
                prompt: Prompt::Text(input),
                artifacts: &[],
            },
            key,
            backend,
            context_options,
            None,
        )?;
        self.create_prepared_task(request)
    }

    /// Starts the same durable state machine as the summary variant.
    pub fn create_task_from_params(&mut self, p: TaskStartParams) -> Result<(Task, bool)> {
        self.create_task_with_memory(p, TaskMemoryQuery::Keyword)
    }

    pub fn create_task_with_memory(
        &mut self,
        p: TaskStartParams,
        query: TaskMemoryQuery<'_>,
    ) -> Result<(Task, bool)> {
        let request = self.prepare_rpc_start(&p)?;
        match self.create_prepared_summary(request, query)? {
            TaskSummaryStart::Existing(summary) => Ok((self.get_task(&summary.id)?, false)),
            TaskSummaryStart::Created(task) => Ok((*task, true)),
        }
    }

    /// Return an eligible workspace only for fresh model tasks. The host can release
    /// its kernel lock before making a network request, then submit normally.
    pub fn task_memory_workspace(&self, p: &TaskStartParams) -> Result<Option<String>> {
        let request = self.prepare_rpc_start(p)?;
        if request.backend != TaskBackend::Model
            || request.options.disable_memory
            || request.options.memory_selection.is_some()
            || request.options.mode == ContextMode::Isolated
            || request.options.max_bytes == 0
        {
            return Ok(None);
        }
        if let Some(key) = request.key
            && !matches!(self.lookup_task_start(&request, key)?, StartLookup::Missing)
        {
            return Ok(None);
        }
        request.input.prompt.validate_media()?;
        Ok(Some(self.get_session(request.session)?.workspace_id))
    }

    pub(crate) fn create_run_child(
        &mut self,
        owner: storage::RunChild<'_>,
        session: &str,
        input: &str,
        options: ContextOptions,
        max_model_steps: Option<u32>,
    ) -> Result<Task> {
        let request = self.prepare_task_start(
            session,
            StartInput {
                prompt: Prompt::Text(input),
                artifacts: &[],
            },
            None,
            TaskBackend::Model,
            Some(options),
            max_model_steps,
        )?;
        self.create_fresh_task(request, Some(owner), TaskMemoryQuery::Keyword, None)
    }

    fn create_prepared_task(&mut self, request: StartRequest<'_>) -> Result<(Task, bool)> {
        match self.create_prepared_summary(request, TaskMemoryQuery::Keyword)? {
            TaskSummaryStart::Existing(summary) => Ok((self.get_task(&summary.id)?, false)),
            TaskSummaryStart::Created(task) => Ok((*task, true)),
        }
    }

    /// Unless the binding predates the compact receipt, a duplicate summary
    /// request never reads the task snapshot. Never schedule an existing task.
    pub fn create_task_for_summary(&mut self, params: TaskStartParams) -> Result<TaskSummaryStart> {
        self.create_summary_with_memory(params, TaskMemoryQuery::Keyword)
    }

    pub fn create_summary_with_memory(
        &mut self,
        p: TaskStartParams,
        query: TaskMemoryQuery<'_>,
    ) -> Result<TaskSummaryStart> {
        let request = self.prepare_rpc_start(&p)?;
        self.create_prepared_summary(request, query)
    }

    /// Store a validated model draft without querying providers or allocating a task.
    pub fn enqueue_task_prompt(&mut self, p: TaskStartParams) -> Result<TaskQueueItem> {
        let request = self.prepare_rpc_start(&p)?;
        let key = request
            .key
            .ok_or_else(|| Error::Invalid("queued prompts require clientRequestId".into()))?;
        let Prompt::Text(text) = request.input.prompt else {
            return Err(Error::Invalid("queued prompts require text input".into()));
        };
        if request.backend != TaskBackend::Model {
            return Err(Error::Invalid(
                "queued prompts require the model backend".into(),
            ));
        }
        let payload = serde_json::to_string(&p)?;
        if payload.len() > MAX_APP_REQUEST_BYTES {
            return Err(Error::Invalid(
                "queued prompt exceeds the request size limit".into(),
            ));
        }
        let preview = text.chars().take(160).collect::<String>();
        let plan_only = request.options.plan_only;
        let digest: [u8; 32] = Sha256::digest(payload.as_bytes()).into();
        self.store_queued_prompt(
            &p.session_id,
            storage::task_queue::NewQueuedPrompt {
                key,
                payload: &payload,
                digest,
                preview,
                plan_only,
            },
        )
    }

    pub fn start_queued_prompt(
        &mut self,
        target: &TaskQueueItemParams,
        query: TaskMemoryQuery<'_>,
    ) -> Result<(TaskQueueItem, Option<Task>)> {
        let (mut item, params) = self.queued_prompt_to_start(target)?;
        let Some(params) = params else {
            return Ok((item, None));
        };
        let request = self.prepare_rpc_start(&params)?;
        let task = self.create_fresh_task(request, None, query, Some(&target.queue_id))?;
        item.status = TaskQueueState::Started {
            task_id: task.id.clone(),
        };
        Ok((item, Some(task)))
    }

    fn prepare_rpc_start<'a>(&self, p: &'a TaskStartParams) -> Result<StartRequest<'a>> {
        self.prepare_task_start(
            &p.session_id,
            StartInput {
                prompt: (&p.input).into(),
                artifacts: &p.artifact_inputs,
            },
            p.client_request_id.as_deref(),
            p.backend.clone(),
            p.context.clone(),
            p.max_model_steps,
        )
    }

    fn create_prepared_summary(
        &mut self,
        request: StartRequest<'_>,
        query: TaskMemoryQuery<'_>,
    ) -> Result<TaskSummaryStart> {
        let lookup = request.key.map_or(Ok(StartLookup::Missing), |key| {
            self.lookup_task_start(&request, key)
        });
        if let Ok(StartLookup::Existing(summary)) = lookup {
            // The receipt was issued only after validating identical input.
            return Ok(TaskSummaryStart::Existing(summary));
        }
        // Keep validation for new requests, legacy bindings and mismatches.
        request.input.prompt.validate_media()?;
        if matches!(lookup?, StartLookup::Legacy) {
            let task = self
                .find_matching_task_start(&request)?
                .ok_or_else(|| Error::NotFound("task request".into()))?;
            let summary = self.get_task_summary(&task.id)?;
            return Ok(TaskSummaryStart::Existing(summary));
        }
        Ok(TaskSummaryStart::Created(Box::new(
            self.create_fresh_task(request, None, query, None)?,
        )))
    }

    fn prepare_task_start<'a>(
        &self,
        session: &'a str,
        input: StartInput<'a>,
        key: Option<&'a str>,
        backend: TaskBackend,
        options: Option<ContextOptions>,
        max_model_steps: Option<u32>,
    ) -> Result<StartRequest<'a>> {
        input.validate_limits(&backend)?;
        self.require_session(session)?;
        if let Some(limit) = max_model_steps {
            if backend != TaskBackend::Model {
                return Err(Error::Invalid(
                    "maxModelSteps applies only to model tasks".into(),
                ));
            }
            if !(1..=MAX_TASK_MODEL_STEPS).contains(&limit) {
                return Err(Error::Invalid(format!(
                    "maxModelSteps must be 1 to {MAX_TASK_MODEL_STEPS}"
                )));
            }
        }
        // An explicit default is the same request as an omitted limit, even
        // for legacy task snapshots and compact start receipts.
        let max_model_steps = max_model_steps.filter(|limit| *limit != DEFAULT_TASK_MODEL_STEPS);
        let explicit_context = options.is_some();
        let options = context::normalize_options(options.unwrap_or_default())?;
        if options.project_guide_revision.is_some() && backend != TaskBackend::Model {
            return Err(Error::Invalid(
                "projectGuideRevision applies only to model tasks".into(),
            ));
        }
        if options.plan_only && backend != TaskBackend::Model {
            return Err(Error::Invalid(
                "planOnly applies only to model tasks".into(),
            ));
        }
        if options.tool_scope != ToolScope::All && backend != TaskBackend::Model {
            return Err(Error::Invalid(
                "toolScope applies only to model tasks".into(),
            ));
        }
        if let Some(key) = key {
            validate_key(key)?;
        }
        Ok(StartRequest {
            session,
            input,
            key,
            backend,
            options,
            max_model_steps,
            explicit_context,
        })
    }

    fn find_matching_task_start(&self, request: &StartRequest<'_>) -> Result<Option<Task>> {
        let Some(key) = request.key else {
            return Ok(None);
        };
        let Some(task) = self.find_task_request(request.session, key)? else {
            return Ok(None);
        };
        if self.has_approval_binding(request.session, key)? {
            return Err(Error::Conflict(
                "request ID already belongs to a resumed run".into(),
            ));
        }
        let same_input = request.input.artifacts.is_empty()
            && request.input.prompt.matches(task.messages.first());
        let same_context = task
            .context
            .as_ref()
            .map_or(!request.explicit_context, |context| {
                context.options == request.options
            });
        if same_input
            && task.backend == request.backend
            && same_context
            && task.max_model_steps == request.max_model_steps
            && task.remote_continuation.is_none()
            && task.agent_session_continuation.is_none()
        {
            Ok(Some(task))
        } else {
            Err(request_conflict())
        }
    }

    fn create_fresh_task(
        &mut self,
        request: StartRequest<'_>,
        owner: Option<storage::RunChild<'_>>,
        query: TaskMemoryQuery<'_>,
        queued_prompt: Option<&str>,
    ) -> Result<Task> {
        let StartRequest {
            session,
            input,
            key,
            backend,
            options,
            max_model_steps,
            ..
        } = request;
        let backend_authority = match &backend {
            TaskBackend::Model => None,
            TaskBackend::ExternalAgent { agent_id } => Some(
                self.agents
                    .get(agent_id)
                    .ok_or_else(|| Error::NotFound("registered agent".into()))?
                    .authority(),
            ),
        };
        let message = input.message(self, session)?;
        let id = new_id("task");
        let mut context = self.build_session_context(session, options, None)?;
        let memory_query = match &message {
            Message::User { text } => text.clone(),
            Message::UserParts { parts } => parts
                .iter()
                .map(UserContentPart::text_projection)
                .collect::<String>(),
            _ => String::new(),
        };
        let memory_access =
            self.inject_context_memories(session, &memory_query, &mut context, query)?;
        let task = Task {
            id,
            session_id: session.into(),
            status: TaskStatus::Running,
            messages: vec![message],
            context: Some(context),
            pending_approval: None,
            in_flight_tool: None,
            pending_tool_input: None,
            model_outcome_unknown: false,
            active_reply_identity: None,
            context_memory_omitted_for_budget: false,
            output: None,
            model_steps: 0,
            max_model_steps,
            backend,
            backend_authority,
            agent_run: None,
            remote_continuation: None,
            agent_session_continuation: None,
        };
        self.commit_task_start(
            &task,
            key.map(|key| (key, input.digest())),
            memory_access.as_ref(),
            owner,
            queued_prompt,
        )?;
        Ok(task)
    }
}
