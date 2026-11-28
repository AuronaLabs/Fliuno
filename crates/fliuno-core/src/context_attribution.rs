use super::*;
use rusqlite::{OptionalExtension, Transaction, params};

pub(crate) fn migrate(db: &Connection) -> Result<()> {
    db.execute_batch(
        "BEGIN IMMEDIATE;
        CREATE TABLE IF NOT EXISTS workspace_instructions(
            workspace_id TEXT PRIMARY KEY REFERENCES workspaces(id),
            revision INTEGER NOT NULL,
            text TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS task_context_evidence(
            task_id TEXT PRIMARY KEY REFERENCES tasks(id),
            preflight_sequence INTEGER REFERENCES events(sequence),
            rejection_sequence INTEGER REFERENCES events(sequence)
        );
        PRAGMA user_version=73; COMMIT;",
    )?;
    Ok(())
}

pub(crate) fn record(
    tx: &Transaction<'_>,
    task_id: &str,
    event: &EventKind,
    sequence: i64,
) -> Result<()> {
    let sql = match event {
        EventKind::ModelPreflightObserved { .. } => {
            "INSERT INTO task_context_evidence(task_id, preflight_sequence) VALUES(?1,?2)
             ON CONFLICT(task_id) DO UPDATE SET preflight_sequence=excluded.preflight_sequence"
        }
        EventKind::ModelBudgetRejected { .. } => {
            "INSERT INTO task_context_evidence(task_id, rejection_sequence) VALUES(?1,?2)
             ON CONFLICT(task_id) DO UPDATE SET rejection_sequence=excluded.rejection_sequence"
        }
        _ => return Ok(()),
    };
    tx.prepare_cached(sql)?
        .execute(params![task_id, sequence])?;
    Ok(())
}

impl Kernel {
    pub fn workspace_instructions(
        &self,
        workspace_id: &str,
    ) -> Result<Option<WorkspaceInstructions>> {
        self.get_workspace(workspace_id)?;
        self.db
            .prepare_cached(
                "SELECT revision,text FROM workspace_instructions WHERE workspace_id=?1",
            )?
            .query_row([workspace_id], |row| {
                Ok(WorkspaceInstructions {
                    workspace_id: workspace_id.into(),
                    revision: row.get::<_, i64>(0)? as u64,
                    text: row.get(1)?,
                })
            })
            .optional()
            .map_err(Into::into)
    }

    pub fn set_workspace_instructions(
        &mut self,
        p: WorkspaceSetInstructionsParams,
    ) -> Result<WorkspaceInstructions> {
        if p.text.len() > 8192 || p.expected_revision >= i64::MAX as u64 {
            return Err(Error::Invalid(
                "invalid instructions revision or text over 8192 UTF-8 bytes".into(),
            ));
        }
        let current = self.workspace_instructions(&p.workspace_id)?;
        if let Some(item) = &current
            && item.revision == p.expected_revision + 1
            && item.text == p.text
        {
            return Ok(item.clone());
        }
        if current.as_ref().map_or(0, |item| item.revision) != p.expected_revision {
            return Err(Error::Conflict("workspace instructions changed".into()));
        }
        let revision = p.expected_revision + 1;
        self.db.execute(
            "INSERT INTO workspace_instructions VALUES(?1,?2,?3)
            ON CONFLICT(workspace_id) DO UPDATE SET revision=excluded.revision,text=excluded.text",
            params![p.workspace_id, revision as i64, p.text],
        )?;
        Ok(WorkspaceInstructions {
            workspace_id: p.workspace_id,
            revision,
            text: p.text,
        })
    }

    /// Estimate a prospective text draft using only local selection and registered tool specs.
    /// No model calls, memory access audit, or task writes take place.
    pub fn inspect_draft_context(&self, p: DraftContextParams) -> Result<DraftContextReport> {
        if p.input.len() > MAX_TASK_TEXT_BYTES {
            return Err(Error::Invalid(
                "draft input exceeds 262144 UTF-8 bytes".into(),
            ));
        }
        let mut context = self.build_session_context(&p.session_id, p.options, None)?;
        let memory_mode =
            if context.options.mode == ContextMode::Isolated || context.options.disable_memory {
                DraftMemoryMode::Off
            } else if context.options.memory_selection.is_some() {
                DraftMemoryMode::Explicit
            } else {
                DraftMemoryMode::KeywordPreview
            };
        self.inject_context_memories(
            &p.session_id,
            &p.input,
            &mut context,
            TaskMemoryQuery::Keyword,
        )?;
        let tools = self.draft_model_specs(&context.options);
        let messages = if p.input.is_empty() {
            None
        } else {
            Some(Message::User { text: p.input })
        };
        // The transient snapshot is never assigned an execution identity or committed.
        let draft = Task {
            id: String::new(),
            session_id: p.session_id,
            status: TaskStatus::Running,
            messages: messages.into_iter().collect(),
            context: Some(context),
            pending_approval: None,
            in_flight_tool: None,
            pending_tool_input: None,
            model_outcome_unknown: false,
            active_reply_identity: None,
            context_memory_omitted_for_budget: false,
            output: None,
            model_steps: 0,
            max_model_steps: None,
            backend: TaskBackend::Model,
            backend_authority: None,
            agent_run: None,
            remote_continuation: None,
            agent_session_continuation: None,
        };
        let request_size = self
            .provider
            .draft_request_size(&draft, &tools)
            .map_err(|_| Error::Invalid("cannot encode the prospective model request".into()))?;
        let context = draft.context.as_ref().expect("draft context is installed");
        Ok(DraftContextReport {
            through_sequence: context.through_sequence,
            model: self.provider.reply_identity(),
            budget: self.configured_model_budget(),
            history_turns: context.turns.len() as u32,
            omitted_turns: context.omitted_turns,
            memory_mode,
            memory_count: context.memories.len() as u32,
            tool_count: tools.len() as u32,
            instructions_bytes: context
                .instructions
                .as_ref()
                .map(|instructions| instructions.text.len() as u64)
                .unwrap_or(0),
            project_guide: context
                .project_guide
                .as_ref()
                .map(ProjectGuideAttribution::from),
            normalized_input_bytes: crate::provider::input_attribution(&draft, &tools)?,
            request_size,
        })
    }

    pub fn task_context_attribution(&self, task_id: &str) -> Result<TaskContextAttribution> {
        let task = self.get_task(task_id)?;
        let (preflight, rejection): (Option<i64>, Option<i64>) = self.db.prepare_cached(
            "SELECT preflight_sequence,rejection_sequence FROM task_context_evidence WHERE task_id=?1"
        )?.query_row([task_id], |row| Ok((row.get(0)?, row.get(1)?))).optional()?.unwrap_or_default();
        let context = task.context.as_ref();
        let instructions = context.and_then(|ctx| ctx.instructions.as_ref());
        let mut memories = Vec::new();
        for memory in context.into_iter().flat_map(|ctx| &ctx.memories) {
            memories.push(ContextMemoryAttribution {
                id: memory.id.clone(),
                revision: memory.revision,
                matched_by: memory.matched_by,
                source_task_id: memory.source_task_id.clone(),
                normalized_bytes: json_size(memory)? as u64,
                omitted_for_budget: task.context_memory_omitted_for_budget,
            });
        }
        Ok(TaskContextAttribution {
            task_id: task.id.clone(),
            model_steps: task.model_steps,
            history: context
                .into_iter()
                .flat_map(|ctx| &ctx.turns)
                .map(|turn| ContextSummarySource {
                    task_id: turn.task_id.clone(),
                    completion_sequence: turn.completion_sequence,
                })
                .collect(),
            omitted_history: context.map_or(0, |ctx| ctx.omitted_turns),
            summarized_history: context
                .into_iter()
                .flat_map(|ctx| &ctx.summaries)
                .flat_map(|summary| summary.sources.iter().cloned())
                .collect(),
            memories,
            instructions_revision: instructions.map(|value| value.revision),
            instructions_bytes: instructions
                .map(|value| json_size(&value.text))
                .transpose()?
                .unwrap_or(0) as u64,
            project_guide: context
                .and_then(|ctx| ctx.project_guide.as_ref())
                .map(ProjectGuideAttribution::from),
            normalized_input_bytes: if task.backend == TaskBackend::Model {
                Some(crate::provider::input_attribution(
                    &task,
                    &self.bound_model_specs(&task)?,
                )?)
            } else {
                None
            },
            latest_preflight: preflight
                .map(|seq| self.get_event(seq as u64))
                .transpose()?,
            latest_budget_rejection: rejection
                .map(|seq| self.get_event(seq as u64))
                .transpose()?,
        })
    }
}
