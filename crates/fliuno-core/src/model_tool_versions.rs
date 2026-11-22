use super::*;
use rusqlite::{OptionalExtension, Transaction, params};
use serde_json::json;

type Bindings = BTreeMap<String, String>;

pub(crate) fn migrate(db: &Connection) -> Result<()> {
    db.execute_batch("BEGIN IMMEDIATE;
        CREATE TABLE IF NOT EXISTS model_tool_sets(id TEXT PRIMARY KEY, data TEXT NOT NULL);
        CREATE TABLE IF NOT EXISTS model_task_tool_sets(
            task_id TEXT PRIMARY KEY REFERENCES tasks(id), set_id TEXT NOT NULL REFERENCES model_tool_sets(id)
        );
        CREATE TABLE IF NOT EXISTS model_tool_revocations(
            name TEXT NOT NULL, authority TEXT NOT NULL, PRIMARY KEY(name, authority)
        );
        PRAGMA user_version=74; COMMIT;")?;
    Ok(())
}

pub(crate) fn record(tx: &Transaction<'_>, task_id: &str, bindings: &Bindings) -> Result<()> {
    let id = authority_digest("model-tool-set.v1", &json!(bindings));
    tx.prepare_cached("INSERT OR IGNORE INTO model_tool_sets VALUES(?1,?2)")?
        .execute(params![id, serde_json::to_string(bindings)?])?;
    tx.prepare_cached("INSERT INTO model_task_tool_sets VALUES(?1,?2)")?
        .execute(params![task_id, id])?;
    Ok(())
}

pub(crate) fn load_revocations(db: &Connection) -> Result<BTreeMap<String, BTreeSet<String>>> {
    let mut select = db.prepare("SELECT name, authority FROM model_tool_revocations")?;
    let mut rows = select.query([])?;
    let mut revoked = BTreeMap::<String, BTreeSet<String>>::new();
    while let Some(row) = rows.next()? {
        revoked.entry(row.get(0)?).or_default().insert(row.get(1)?);
    }
    Ok(revoked)
}

impl Kernel {
    /// Replace an existing model tool for new tasks. External Agent host tools
    /// remain on their own registered adapter. Existing model tasks retain a
    /// reference to the original authority, including pre-replacement approvals.
    pub fn activate_model_tool_version(
        &mut self,
        tool: Box<dyn Tool>,
        expected_authority: &str,
    ) -> Result<String> {
        let name = tool.spec().name;
        let base = self
            .tools
            .get(&name)
            .filter(|_| !crate::interaction::is_internal(&name))
            .ok_or_else(|| Error::NotFound("replaceable model tool".into()))?;
        let previous = self.model_tool_overrides.get(&name).unwrap_or(base);
        let previous_authority = previous.authority();
        let authority = tool.authority();
        if expected_authority != previous_authority {
            return Err(Error::Conflict(
                "active model tool authority changed".into(),
            ));
        }
        if !crate::authority::valid(&authority)
            || authority == previous_authority
            || self.model_tool_revoked(&name, &authority)
        {
            return Err(Error::Conflict(
                "replacement must have a different, valid and unrevoked authority".into(),
            ));
        }
        // Check for an existing durable Run to avoid invalidating a scheduled
        // graph's static tool and policy identity.
        let active_run: bool = self.db.query_row(
            "SELECT EXISTS(SELECT 1 FROM runs WHERE status='running')",
            [],
            |row| row.get(0),
        )?;
        if active_run {
            return Err(Error::Conflict(
                "wait for active Runs to settle before activating a model tool version".into(),
            ));
        }
        let previous = Arc::clone(previous);
        self.retained_model_tools
            .insert((name.clone(), previous_authority), previous);
        self.model_tool_overrides.insert(name, Arc::from(tool));
        Ok(authority)
    }

    /// Restore a host-provided historical tool definition after a restart.
    pub fn retain_model_tool_version(&mut self, tool: Box<dyn Tool>) -> Result<()> {
        let name = tool.spec().name;
        let authority = tool.authority();
        let base = self
            .tools
            .get(&name)
            .filter(|_| !crate::interaction::is_internal(&name))
            .ok_or_else(|| Error::NotFound("replaceable model tool".into()))?;
        if !crate::authority::valid(&authority) || self.model_tool_revoked(&name, &authority) {
            return Err(Error::Conflict(
                "historical model tool authority is invalid or revoked".into(),
            ));
        }
        let active = self.model_tool_overrides.get(&name).unwrap_or(base);
        if authority == base.authority()
            || authority == active.authority()
            || self
                .retained_model_tools
                .contains_key(&(name.clone(), authority.clone()))
        {
            return Err(Error::Conflict(
                "model tool version is already registered".into(),
            ));
        }
        self.retained_model_tools
            .insert((name, authority), Arc::from(tool));
        Ok(())
    }

    /// Revoke an authority for subsequent model dispatch, including in-flight
    /// preparations that have not acquired a durable ToolStarted record.
    pub fn revoke_model_tool_version(&mut self, name: &str, authority: &str) -> Result<()> {
        if !self.tools.contains_key(name)
            || crate::interaction::is_internal(name)
            || !crate::authority::valid(authority)
        {
            return Err(Error::Invalid("invalid model tool version".into()));
        }
        self.db.execute(
            "INSERT OR IGNORE INTO model_tool_revocations VALUES(?1,?2)",
            params![name, authority],
        )?;
        self.revoked_model_tools
            .entry(name.to_owned())
            .or_default()
            .insert(authority.to_owned());
        self.retained_model_tools
            .remove(&(name.to_owned(), authority.to_owned()));
        Ok(())
    }

    pub(crate) fn model_tool_revoked(&self, name: &str, authority: &str) -> bool {
        self.revoked_model_tools
            .get(name)
            .is_some_and(|versions| versions.contains(authority))
    }

    pub(crate) fn active_model_tools(&self) -> impl Iterator<Item = (&String, &Arc<dyn Tool>)> {
        self.tools
            .iter()
            .map(|(name, tool)| (name, self.model_tool_overrides.get(name).unwrap_or(tool)))
    }

    pub(crate) fn current_model_bindings(&self, task: &Task) -> Result<Bindings> {
        let mut bindings = BTreeMap::new();
        for (name, tool) in self.active_model_tools() {
            if crate::interaction::is_internal(name) && !crate::interaction::enabled(task) {
                continue;
            }
            let authority = tool.authority();
            if !self.model_tool_revoked(name, &authority) {
                bindings.insert(name.clone(), authority);
            }
        }
        Ok(bindings)
    }

    fn model_bindings(&self, task_id: &str) -> Result<Option<Bindings>> {
        let raw = self
            .db
            .prepare_cached(
                "SELECT s.data FROM model_task_tool_sets b
            JOIN model_tool_sets s ON s.id=b.set_id WHERE b.task_id=?1",
            )?
            .query_row([task_id], |row| row.get::<_, String>(0))
            .optional()?;
        raw.map(|raw| serde_json::from_str(&raw).map_err(Into::into))
            .transpose()
    }

    fn lookup_model_version(&self, name: &str, authority: &str) -> Result<Option<Arc<dyn Tool>>> {
        if self.model_tool_revoked(name, authority) {
            return Ok(None);
        }
        let result = [self.model_tool_overrides.get(name), self.tools.get(name)]
            .into_iter()
            .flatten()
            .find(|tool| tool.authority() == authority)
            .or_else(|| {
                self.retained_model_tools
                    .get(&(name.to_owned(), authority.to_owned()))
            });
        Ok(result.cloned())
    }

    pub(crate) fn bound_model_tool(
        &self,
        task: &Task,
        name: &str,
    ) -> Result<Option<Arc<dyn Tool>>> {
        if crate::interaction::is_internal(name) && !crate::interaction::enabled(task) {
            return Ok(None);
        }
        if let Some(bindings) = self.model_bindings(&task.id)? {
            return match bindings.get(name) {
                Some(authority) => self.lookup_model_version(name, authority),
                None => Ok(None),
            };
        }
        let Some(tool) = self.tools.get(name) else {
            return Ok(None);
        };
        if self.model_tool_revoked(name, &tool.authority()) {
            return Ok(None);
        }
        Ok(Some(tool.clone()))
    }

    pub(crate) fn bound_model_specs(&self, task: &Task) -> Result<Vec<ToolSpec>> {
        if let Some(bindings) = self.model_bindings(&task.id)? {
            let mut specs = Vec::with_capacity(bindings.len());
            for (name, authority) in bindings {
                if let Some(tool) = self.lookup_model_version(&name, &authority)? {
                    specs.push(tool.spec());
                }
            }
            return Ok(specs);
        }
        let mut specs = Vec::new();
        for (name, tool) in &self.tools {
            if (crate::interaction::enabled(task) || !crate::interaction::is_internal(name))
                && !self.model_tool_revoked(name, &tool.authority())
            {
                specs.push(tool.spec());
            }
        }
        Ok(specs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct VersionedTool(&'static str, Arc<AtomicUsize>);

    impl Tool for VersionedTool {
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: "versioned".into(),
                description: self.0.into(),
                effect: ToolEffect::WorkspaceRead,
                input_schema: json!({"type":"object"}),
            }
        }
        fn validate(&self, _: &Value) -> std::result::Result<(), String> {
            Ok(())
        }
        fn execute(
            &self,
            _: &ToolContext<'_>,
            _: &ToolCall,
            _: &CancellationToken,
        ) -> std::result::Result<ToolOutput, ToolError> {
            self.1.fetch_add(1, Ordering::Relaxed);
            Ok(ToolOutput {
                output: json!({"version":self.0}),
                artifacts: vec![],
            })
        }
    }

    struct Fixture;
    impl ModelProvider for Fixture {
        fn next(
            &self,
            task: &Task,
            _: &[ToolSpec],
            _: &CancellationToken,
        ) -> std::result::Result<ModelTurn, String> {
            if let Some(version) = task.messages.iter().find_map(|msg| match msg {
                Message::ToolResult { output, .. } => output["version"].as_str(),
                _ => None,
            }) {
                Ok(ModelDecision::Complete(version.into()).into())
            } else {
                Ok(ModelDecision::Call(ToolCall {
                    id: "call".into(),
                    name: "versioned".into(),
                    arguments: json!({}),
                })
                .into())
            }
        }
    }

    #[test]
    fn changed_tool_preserves_old_task_binding_and_revocation_stops_prepared_dispatch() {
        let first = Arc::new(AtomicUsize::new(0));
        let second = Arc::new(AtomicUsize::new(0));
        let mut kernel = Kernel::in_memory(Box::new(Fixture)).unwrap();
        let old_tool = VersionedTool("v1", Arc::clone(&first));
        let old_authority = old_tool.authority();
        kernel.register_tool(Box::new(old_tool)).unwrap();
        let workspace = kernel
            .open_workspace(std::env::current_dir().unwrap().to_str().unwrap())
            .unwrap();
        let session = kernel.create_session(&workspace.id).unwrap();
        let old = kernel.start_task(&session.id, "old").unwrap();
        let doomed = kernel.start_task(&session.id, "revoke").unwrap();
        let new_authority = kernel
            .activate_model_tool_version(
                Box::new(VersionedTool("v2", Arc::clone(&second))),
                &old_authority,
            )
            .unwrap();
        let new = kernel.start_task(&session.id, "new").unwrap();
        assert_eq!(
            new.pending_approval.as_ref().unwrap().authority.as_deref(),
            Some(new_authority.as_str())
        );
        for (task, version) in [(old, "v1"), (new, "v2")] {
            let complete = kernel
                .respond_approval(
                    &task.id,
                    &task.pending_approval.unwrap().id,
                    ApprovalDecision::AllowOnce,
                )
                .unwrap();
            assert_eq!(complete.output.as_deref(), Some(version));
        }
        let ApprovalOutcome::Prepared { execution, .. } = kernel
            .prepare_approval(
                &doomed.id,
                &doomed.pending_approval.unwrap().id,
                ApprovalDecision::AllowOnce,
            )
            .unwrap()
        else {
            panic!("expected old version to remain available");
        };
        kernel
            .revoke_model_tool_version("versioned", &old_authority)
            .unwrap();
        assert!(!kernel.begin_tool_execution(&execution).unwrap());
        assert_eq!(
            kernel.get_task(&doomed.id).unwrap().status,
            TaskStatus::Failed
        );
        assert_eq!(first.load(Ordering::Relaxed), 1);
        assert_eq!(second.load(Ordering::Relaxed), 1);
    }
}
