use super::*;
use fliuno_protocol::{
    PolicyReadRule, PolicyReadRuleCreateParams, PolicyReadRuleRevokeParams, PolicyReadTool, Task,
};
use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};

const MAX_RULE_LIFETIME_MS: i64 = 30 * 24 * 60 * 60 * 1000;
const MAX_RULES_PER_WORKSPACE: u32 = 64;
const MAX_RULE_ARGUMENT_BYTES: usize = 4096;

fn now_ms() -> Result<i64> {
    i64::try_from(agent::now_ms()?)
        .map_err(|_| Error::Conflict("system time exceeds SQLite integer range".into()))
}

fn require_id(value: &str, label: &str) -> Result<()> {
    if value.is_empty() || value.len() > 128 || !value.bytes().all(|byte| byte.is_ascii_graphic()) {
        return Err(Error::Invalid(format!("invalid {label}")));
    }
    Ok(())
}

fn arguments_digest(arguments: &Value) -> Result<Option<String>> {
    if !arguments.is_object() || json_size(arguments)? > MAX_RULE_ARGUMENT_BYTES {
        return Ok(None);
    }
    Ok(Some(authority_digest(
        "policy.read-arguments.v1",
        arguments,
    )))
}

fn read_rule(data: &str, revoked_at_ms: Option<i64>) -> Result<PolicyReadRule> {
    let mut rule: PolicyReadRule = serde_json::from_str(data)?;
    rule.revoked_at_ms = revoked_at_ms;
    Ok(rule)
}

fn grantable(tool: &dyn Tool) -> bool {
    tool.supports_read_rules() && matches!(tool.spec().effect, ToolEffect::WorkspaceRead)
}

impl Kernel {
    pub fn policy_read_tools(&self) -> Vec<PolicyReadTool> {
        self.tools
            .iter()
            .filter(|(_, tool)| grantable(tool.as_ref()))
            .map(|(name, tool)| PolicyReadTool {
                name: name.clone(),
                authority: tool.authority(),
            })
            .collect()
    }

    pub fn create_policy_read_rule(
        &mut self,
        p: PolicyReadRuleCreateParams,
    ) -> Result<PolicyReadRule> {
        require_id(&p.workspace_id, "workspace ID")?;
        require_id(&p.client_request_id, "policy request ID")?;
        require_id(&p.tool_name, "tool name")?;
        if !crate::authority::valid(&p.expected_authority) {
            return Err(Error::Invalid("invalid tool authority".into()));
        }
        self.get_workspace(&p.workspace_id)?;
        if let Some(session_id) = &p.session_id {
            require_id(session_id, "session ID")?;
            if self.get_session(session_id)?.workspace_id != p.workspace_id {
                return Err(Error::Invalid(
                    "session is outside the rule workspace".into(),
                ));
            }
        }
        let digest = arguments_digest(&p.arguments)?.ok_or_else(|| {
            Error::Invalid("rule arguments must be an object up to 4096 bytes".into())
        })?;
        let fingerprint = authority_digest("policy.read-rule-request.v1", &json!(p));
        let existing = self
            .db
            .query_row(
                "SELECT request_digest, data, revoked_at_ms FROM policy_read_rules
             WHERE workspace_id=?1 AND request_id=?2",
                params![p.workspace_id, p.client_request_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get(2)?,
                    ))
                },
            )
            .optional()?;
        if let Some((previous, data, revoked)) = existing {
            return if previous == fingerprint {
                read_rule(&data, revoked)
            } else {
                Err(Error::Conflict(
                    "policy request ID was used for different arguments".into(),
                ))
            };
        }
        let now = now_ms()?;
        if p.expires_at_ms <= now || p.expires_at_ms > now.saturating_add(MAX_RULE_LIFETIME_MS) {
            return Err(Error::Invalid(
                "read rule must expire within the next 30 days".into(),
            ));
        }
        let tool = self
            .tools
            .get(&p.tool_name)
            .filter(|tool| grantable(tool.as_ref()))
            .ok_or_else(|| Error::Invalid("tool is ineligible for workspace read rules".into()))?;
        if tool.authority() != p.expected_authority {
            return Err(Error::Conflict(
                "tool registration no longer matches reviewed authority".into(),
            ));
        }
        tool.validate(&p.arguments).map_err(Error::Invalid)?;
        let active: u32 = self.db.query_row(
            "SELECT count(*) FROM policy_read_rules WHERE workspace_id=?1
             AND revoked_at_ms IS NULL AND expires_at_ms>?2",
            params![p.workspace_id, now],
            |row| row.get(0),
        )?;
        if active >= MAX_RULES_PER_WORKSPACE {
            return Err(Error::Conflict("workspace read rule limit reached".into()));
        }
        let rule = PolicyReadRule {
            id: new_id("policy"),
            workspace_id: p.workspace_id,
            session_id: p.session_id,
            tool_name: p.tool_name,
            authority: p.expected_authority,
            arguments: p.arguments,
            expires_at_ms: p.expires_at_ms,
            revoked_at_ms: None,
        };
        self.db.execute(
            "INSERT INTO policy_read_rules (id, workspace_id, session_id, tool_name, authority,
             arguments_digest, expires_at_ms, request_id, request_digest, data)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                rule.id,
                rule.workspace_id,
                rule.session_id,
                rule.tool_name,
                rule.authority,
                digest,
                rule.expires_at_ms,
                p.client_request_id,
                fingerprint,
                serde_json::to_string(&rule)?
            ],
        )?;
        Ok(rule)
    }

    pub fn list_policy_read_rules(&self, workspace_id: &str) -> Result<Vec<PolicyReadRule>> {
        require_id(workspace_id, "workspace ID")?;
        self.get_workspace(workspace_id)?;
        let mut query = self.db.prepare(
            "SELECT data FROM policy_read_rules WHERE workspace_id=?1
             AND revoked_at_ms IS NULL AND expires_at_ms>?2 ORDER BY expires_at_ms, id LIMIT 64",
        )?;
        query
            .query_map(params![workspace_id, now_ms()?], |row| {
                row.get::<_, String>(0)
            })?
            .map(|data| read_rule(&data?, None))
            .collect()
    }

    pub fn revoke_policy_read_rule(
        &mut self,
        p: &PolicyReadRuleRevokeParams,
    ) -> Result<PolicyReadRule> {
        require_id(&p.workspace_id, "workspace ID")?;
        require_id(&p.rule_id, "rule ID")?;
        let found = self
            .db
            .query_row(
                "UPDATE policy_read_rules SET revoked_at_ms=coalesce(revoked_at_ms, ?3)
             WHERE workspace_id=?1 AND id=?2 RETURNING data, revoked_at_ms",
                params![p.workspace_id, p.rule_id, now_ms()?],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<i64>>(1)?)),
            )
            .optional()?;
        let (data, revoked) = found.ok_or_else(|| Error::NotFound("policy read rule".into()))?;
        read_rule(&data, revoked)
    }

    pub(crate) fn match_policy_read_rule(
        &self,
        task: &Task,
        call: &ToolCall,
        authority: &str,
        required_rule_id: Option<&str>,
    ) -> Result<Option<String>> {
        let Some(tool) = self.tools.get(&call.name) else {
            return Ok(None);
        };
        if !grantable(tool.as_ref()) || tool.authority() != authority {
            return Ok(None);
        }
        let Some(digest) = arguments_digest(&call.arguments)? else {
            return Ok(None);
        };
        self.db.query_row(
            "SELECT id FROM policy_read_rules WHERE workspace_id=(SELECT workspace_id FROM sessions WHERE id=?1)
             AND (session_id IS NULL OR session_id=?1) AND tool_name=?2 AND authority=?3
             AND arguments_digest=?4 AND expires_at_ms>?5 AND revoked_at_ms IS NULL
             AND (?6 IS NULL OR id=?6) ORDER BY session_id IS NULL, expires_at_ms, id LIMIT 1",
            params![task.session_id, call.name, authority, digest, now_ms()?, required_rule_id],
            |row| row.get(0),
        ).optional().map_err(Into::into)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct FixtureTool(Arc<AtomicUsize>);

    impl Tool for FixtureTool {
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: "test_read".into(),
                description: "count executions".into(),
                effect: ToolEffect::WorkspaceRead,
                input_schema: json!({"type":"object"}),
            }
        }

        fn supports_read_rules(&self) -> bool {
            true
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
            self.0.fetch_add(1, Ordering::Relaxed);
            Ok(ToolOutput {
                output: json!({}),
                artifacts: vec![],
            })
        }
    }

    #[test]
    fn revoking_a_prepared_rule_requires_new_approval_before_dispatch() {
        let count = Arc::new(AtomicUsize::new(0));
        let mut kernel = Kernel::in_memory(Box::new(DemoProvider)).unwrap();
        let tool = FixtureTool(Arc::clone(&count));
        let authority = tool.authority();
        kernel.register_tool(Box::new(tool)).unwrap();
        let workspace = kernel
            .open_workspace(std::env::current_dir().unwrap().to_str().unwrap())
            .unwrap();
        let session = kernel.create_session(&workspace.id).unwrap();
        let params = PolicyReadRuleCreateParams {
            workspace_id: workspace.id.clone(),
            session_id: Some(session.id.clone()),
            tool_name: "test_read".into(),
            expected_authority: authority,
            arguments: json!({"path":"report.txt"}),
            expires_at_ms: now_ms().unwrap() + 60_000,
            client_request_id: "grant".into(),
        };
        let rule = kernel.create_policy_read_rule(params.clone()).unwrap();
        let (task, _) = kernel
            .create_task_with_key(&session.id, "read the file", None)
            .unwrap();
        let PreparedModelStep::Ready { task, .. } = kernel.prepare_model_step(&task.id).unwrap()
        else {
            panic!("expected model step");
        };
        let task = kernel
            .apply_model_response(
                task,
                Ok(ModelDecision::Call(ToolCall {
                    id: "call".into(),
                    name: params.tool_name.clone(),
                    arguments: params.arguments.clone(),
                })
                .into()),
            )
            .unwrap();
        let previous = task.pending_approval.as_ref().unwrap().id.clone();
        let Some(ApprovalOutcome::Prepared { execution, .. }) =
            kernel.prepare_policy_approval(&task.id).unwrap()
        else {
            panic!("expected policy approval");
        };
        let revoked = kernel
            .revoke_policy_read_rule(&PolicyReadRuleRevokeParams {
                workspace_id: workspace.id,
                rule_id: rule.id,
            })
            .unwrap();
        assert!(revoked.revoked_at_ms.is_some());
        assert_eq!(kernel.create_policy_read_rule(params).unwrap(), revoked);
        assert!(!kernel.begin_tool_execution(&execution).unwrap());
        assert_eq!(count.load(Ordering::Relaxed), 0);
        let waiting = kernel.get_task(&task.id).unwrap();
        assert_eq!(waiting.status, TaskStatus::WaitingApproval);
        let fresh = waiting.pending_approval.unwrap().id;
        assert_ne!(previous, fresh);
        assert!(
            kernel
                .resolve_approval(&task.id, &previous, ApprovalDecision::AllowOnce)
                .is_err()
        );
        let after = kernel
            .resolve_approval(&task.id, &fresh, ApprovalDecision::AllowOnce)
            .unwrap();
        assert_eq!(after.status, TaskStatus::Running);
        assert_eq!(count.load(Ordering::Relaxed), 1);
        let events = kernel.list_events(&task.id, 0, 100).unwrap().events;
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(e.data, EventKind::ToolStarted { .. }))
                .count(),
            1
        );
    }
}
