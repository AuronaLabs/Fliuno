//! Ordinary task submission and durable request identity.
use super::{validate_input, validate_key};
use crate::*;

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
    pub input: &'a str,
    pub key: Option<&'a str>,
    pub backend: TaskBackend,
    pub options: ContextOptions,
    pub max_model_steps: Option<u32>,
    explicit_context: bool,
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
        let request =
            self.prepare_task_start(session_id, input, key, backend, context_options, None)?;
        self.create_prepared_task(request)
    }

    /// Starts the same durable state machine as the summary variant.
    pub fn create_task_from_params(&mut self, p: TaskStartParams) -> Result<(Task, bool)> {
        let request = self.prepare_task_start(
            &p.session_id,
            &p.input,
            p.client_request_id.as_deref(),
            p.backend,
            p.context,
            p.max_model_steps,
        )?;
        self.create_prepared_task(request)
    }

    fn create_prepared_task(&mut self, request: StartRequest<'_>) -> Result<(Task, bool)> {
        if let Some(task) = self.find_matching_task_start(&request)? {
            return Ok((task, false));
        }
        Ok((self.create_fresh_task(request)?, true))
    }

    /// Unless the binding predates the compact receipt, a duplicate summary
    /// request never reads the task snapshot. Never schedule an existing task.
    pub fn create_task_for_summary(&mut self, params: TaskStartParams) -> Result<TaskSummaryStart> {
        let p = params;
        let request = self.prepare_task_start(
            &p.session_id,
            &p.input,
            p.client_request_id.as_deref(),
            p.backend,
            p.context,
            p.max_model_steps,
        )?;
        if let Some(key) = request.key {
            match self.lookup_task_start(&request, key)? {
                StartLookup::Missing => {}
                StartLookup::Existing(summary) => return Ok(TaskSummaryStart::Existing(summary)),
                StartLookup::Legacy => {
                    let task = self
                        .find_matching_task_start(&request)?
                        .ok_or_else(|| Error::NotFound("task request".into()))?;
                    let summary = self.get_task_summary(&task.id)?;
                    return Ok(TaskSummaryStart::Existing(summary));
                }
            }
        }
        Ok(TaskSummaryStart::Created(Box::new(
            self.create_fresh_task(request)?,
        )))
    }

    fn prepare_task_start<'a>(
        &self,
        session: &'a str,
        input: &'a str,
        key: Option<&'a str>,
        backend: TaskBackend,
        options: Option<ContextOptions>,
        max_model_steps: Option<u32>,
    ) -> Result<StartRequest<'a>> {
        validate_input(input)?;
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
        let same_input =
            matches!(task.messages.first(), Some(Message::User { text }) if text == request.input);
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

    fn create_fresh_task(&mut self, request: StartRequest<'_>) -> Result<Task> {
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
        let context = self.session_context(session, options)?;
        let task = Task {
            id: new_id("task"),
            session_id: session.into(),
            status: TaskStatus::Running,
            messages: vec![Message::User { text: input.into() }],
            context: Some(context),
            pending_approval: None,
            in_flight_tool: None,
            model_outcome_unknown: false,
            output: None,
            model_steps: 0,
            max_model_steps,
            backend,
            backend_authority,
            agent_run: None,
            remote_continuation: None,
            agent_session_continuation: None,
        };
        self.commit_task_start(&task, key.map(|key| (key, input)))?;
        Ok(task)
    }
}
