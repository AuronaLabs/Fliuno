use crate::{CancellationToken, new_id};
use fliuno_protocol::{Artifact, ArtifactBody, ToolCall, Workspace};

mod binary_artifact;
pub(crate) use binary_artifact::CreateBinaryArtifact;
use serde_json::{Value, json};

#[derive(Debug, Clone, Copy, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolEffect {
    ArtifactWrite,
    WorkspaceRead,
    SkillRead,
    WorkspaceWrite,
    /// Explicitly registered OS program; authority comes from host configuration.
    ProcessExecution,
    /// Persistent trusted PTY resource; host-scoped and independently approved.
    TerminalControl,
    /// A trusted subprocess/remote service can cause effects outside SQLite.
    External,
}

/// Scope is resolved from the task/session relation by the host. Models and
/// approval clients cannot replace the workspace when executing a tool.
pub struct ToolContext<'a> {
    pub task_id: &'a str,
    /// Resolved by the host, never accepted from model arguments. Long-lived
    /// resources use this scope so another session cannot operate on them.
    pub session_id: &'a str,
    pub workspace: &'a Workspace,
}

pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub effect: ToolEffect,
    pub input_schema: Value,
}

/// A staged internal effect. Commit artifacts, snapshot and events atomically.
pub struct ToolOutput {
    pub output: Value,
    pub artifacts: Vec<Artifact>,
}

/// Known failures must not be confused with a lost external execution result.
#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    #[error("{0}")]
    Failed(String),
    #[error("{0}")]
    OutcomeUnknown(String),
    /// Bounded observed evidence is auditable without implying completion.
    #[error("{message}")]
    OutcomeUnknownWithOutput { message: String, output: Value },
}

impl From<String> for ToolError {
    fn from(message: String) -> Self {
        Self::Failed(message)
    }
}
impl From<&str> for ToolError {
    fn from(message: &str) -> Self {
        Self::Failed(message.into())
    }
}

/// Trusted in-process tools only. This interface is not an OS sandbox.
/// Implementations should observe cancellation during bounded work. External
/// tools receive a durable ToolStarted record before execution and are never
/// retried on recovery.
pub trait Tool: Send + Sync {
    fn spec(&self) -> ToolSpec;
    /// Host adapters with endpoint/process/environment authority must include
    /// their immutable resolved registration in this digest. The default binds
    /// the public in-process contract; bump its domain when semantics change.
    fn authority(&self) -> String {
        let spec = self.spec();
        crate::authority_digest(
            "tool.contract.v1",
            &json!({
                "name":spec.name, "description":spec.description,
                "effect":spec.effect, "inputSchema":spec.input_schema,
            }),
        )
    }
    fn validate(&self, arguments: &Value) -> std::result::Result<(), String>;
    fn execute(
        &self,
        context: &ToolContext<'_>,
        call: &ToolCall,
        cancellation: &CancellationToken,
    ) -> std::result::Result<ToolOutput, ToolError>;
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateArtifactArgs {
    title: String,
    content: String,
}

fn artifact_args(value: &Value) -> std::result::Result<CreateArtifactArgs, String> {
    let args: CreateArtifactArgs = serde_json::from_value(value.clone())
        .map_err(|e| format!("invalid artifact arguments: {e}"))?;
    if args.title.trim().is_empty() || args.title.len() > 256 || args.content.len() > 262_144 {
        return Err("artifact title/content exceeds allowed bounds".into());
    }
    Ok(args)
}

pub(crate) struct CreateArtifact;

impl Tool for CreateArtifact {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "artifact_create".into(),
            description: "Create a reviewable text artifact in the Fliuno database after approval."
                .into(),
            effect: ToolEffect::ArtifactWrite,
            input_schema: json!({"type":"object", "additionalProperties":false,
            "required":["title","content"], "properties":{
                "title":{"type":"string","minLength":1,"maxLength":256},
                "content":{"type":"string","maxLength":262144}
            }}),
        }
    }

    fn validate(&self, arguments: &Value) -> std::result::Result<(), String> {
        artifact_args(arguments).map(|_| ())
    }

    fn execute(
        &self,
        context: &ToolContext<'_>,
        call: &ToolCall,
        cancellation: &CancellationToken,
    ) -> std::result::Result<ToolOutput, ToolError> {
        if cancellation.is_cancelled() {
            return Err(ToolError::Failed("tool cancelled before execution".into()));
        }
        let args = artifact_args(&call.arguments)?;
        let artifact = Artifact {
            id: new_id("artifact"),
            task_id: context.task_id.into(),
            title: args.title,
            media_type: "text/plain; charset=utf-8".into(),
            body: ArtifactBody::Text {
                content: args.content,
            },
        };
        Ok(ToolOutput {
            output: json!({"artifactId": artifact.id}),
            artifacts: vec![artifact],
        })
    }
}
