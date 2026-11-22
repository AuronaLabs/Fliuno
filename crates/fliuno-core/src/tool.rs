use crate::{CancellationToken, new_id};
use fliuno_protocol::{Artifact, ArtifactBody, ToolCall, ToolInputRequest, Workspace};

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
    #[error("tool requires user input")]
    InputRequired {
        requests: Value,
        request_state: Option<String>,
    },
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
    /// Host opt-in for validated, confined and side-effect-free workspace reads.
    fn supports_read_rules(&self) -> bool {
        false
    }

    fn validate(&self, arguments: &Value) -> std::result::Result<(), String>;
    /// Optional immutable approval details; no IO or execution at this stage.
    fn execution_plan(&self, _arguments: &Value) -> Option<Value> {
        None
    }
    fn execute(
        &self,
        context: &ToolContext<'_>,
        call: &ToolCall,
        cancellation: &CancellationToken,
    ) -> std::result::Result<ToolOutput, ToolError>;

    /// Optional host adapter for an approved Run child. Recovery only observes
    /// the ticket and never calls the launch method again.
    fn supports_supervision(&self) -> bool {
        false
    }

    fn launch_supervisor(&self, _ticket: &crate::SupervisorTicket) -> crate::Result<()> {
        Err(crate::Error::Conflict(
            "tool does not support supervision".into(),
        ))
    }

    /// Validate user responses against a request previously returned by this tool.
    fn validate_input(
        &self,
        _request: &ToolInputRequest,
        _responses: &Value,
    ) -> std::result::Result<(), String> {
        Err("tool does not accept resumable user input".into())
    }

    /// Reject unsupported or malformed input requests before exposing them.
    fn validate_input_request(
        &self,
        _request: &ToolInputRequest,
        _request_state: Option<&str>,
    ) -> std::result::Result<(), String> {
        Err("tool does not accept resumable user input".into())
    }

    /// Continue the same approved tool call using its opaque persisted state.
    fn resume(
        &self,
        _context: &ToolContext<'_>,
        _call: &ToolCall,
        _request_state: Option<&str>,
        _responses: &Value,
        _cancellation: &CancellationToken,
    ) -> std::result::Result<ToolOutput, ToolError> {
        Err(ToolError::Failed(
            "tool does not support resumable user input".into(),
        ))
    }
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
