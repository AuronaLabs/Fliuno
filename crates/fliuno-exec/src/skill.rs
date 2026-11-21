//! An offline Skill is an approval-bound process with a fixed script and a
//! single text input/output contract. It cannot grant itself process authority.
use crate::{ProcessIsolation, ProcessReport, Program, ProgramConfig, Termination};
use fliuno_core::{ToolContext, ToolEffect, ToolError, ToolOutput, ToolSpec};
use fliuno_protocol::{Artifact, ArtifactBody, MAX_APPROVAL_EXECUTION_PLAN_BYTES};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

const MAX_STDIN: usize = 16 * 1024;
const TEXT_MEDIA_TYPE: &str = "text/plain";

#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SkillExecution {
    /// Portable staged path beneath the discovered Skill's scripts directory.
    pub script: String,
    pub artifact_title: String,
}

pub(crate) fn validate(config: &ProgramConfig) -> Result<(), String> {
    let Some(skill) = &config.skill else {
        return Ok(());
    };
    if config.isolation == ProcessIsolation::TrustedHost || !config.env_allow.is_empty() {
        return Err("offline Skills require native scratch isolation and an empty envAllow".into());
    }
    if !skill.script.starts_with("scripts/")
        || config.args.last() != Some(&skill.script)
        || config.args.len() > 16
        || config.args.iter().map(String::len).sum::<usize>() > 1024
        || !config
            .assets
            .iter()
            .any(|asset| asset.path == skill.script && !asset.executable)
    {
        return Err("offline Skill requires a pinned script and fixed interpreter arguments ending in its portable script path".into());
    }
    if skill.artifact_title.trim().is_empty()
        || skill.artifact_title.len() > 128
        || skill.artifact_title.chars().any(char::is_control)
        || config.assets.len() > 32
    {
        return Err("invalid offline Skill artifact title or staged file count".into());
    }
    Ok(())
}

pub(crate) fn validate_input(value: &Value, stdin: &str) -> Result<(), String> {
    if !value
        .as_object()
        .is_some_and(|object| object.keys().all(|key| key == "stdin"))
        || stdin.len() > MAX_STDIN
    {
        return Err("offline Skill accepts only stdin, up to 16 KiB of UTF-8".into());
    }
    Ok(())
}

pub(crate) fn spec(program: &Program) -> ToolSpec {
    ToolSpec {
        name: program.tool_name.clone(),
        description: format!(
            "Execute the registered offline Skill '{}'. Pass text as stdin. Requires approval; successful complete UTF-8 stdout becomes a text artifact.",
            program.config.id
        ),
        effect: ToolEffect::ProcessExecution,
        input_schema: json!({"type":"object","additionalProperties":false,"properties":{
            "stdin":{"type":"string","maxLength":MAX_STDIN,"description":"UTF-8 stdin, at most 16 KiB encoded."}
        }}),
    }
}

impl SkillExecution {
    pub(crate) fn validate_approval_plan(&self, program: &Program) -> Result<(), String> {
        if self.plan(program, MAX_STDIN).to_string().len() > MAX_APPROVAL_EXECUTION_PLAN_BYTES {
            return Err("offline Skill approval plan exceeds 8 KiB; shorten fixed arguments or staged paths".into());
        }
        Ok(())
    }

    pub(crate) fn plan(&self, program: &Program, stdin_bytes: usize) -> Value {
        let config = &program.config;
        let script = config.assets.iter().find(|asset| asset.path == self.script);
        json!({
            "kind":"offline_skill", "skill": config.id,
            "script": self.script, "scriptSha256":script.map(|asset| &asset.sha256),
            "interpreter":config.command.file_name().and_then(|name| name.to_str()),
            "interpreterSha256":program.executable_digest, "args": config.args,
            "stagedFiles":config.assets.iter().map(|asset| json!({"path":asset.path,"sha256":asset.sha256,"executable":asset.executable})).collect::<Vec<_>>(),
            "isolation":config.isolation, "network":"no_network_grant", "workspaceAccess":false,
            "input":{"source":"stdin","encoding":"utf8","bytes":stdin_bytes,"maxBytes":MAX_STDIN},
            "output":{"source":"stdout","completeUtf8":true,"mediaType":TEXT_MEDIA_TYPE,"title":self.artifact_title,"maxBytes":config.output_bytes},
            "stderrMaxBytes":config.output_bytes, "timeoutMs":config.timeout_ms, "resources":config.resources,
        })
    }

    pub(crate) fn finish(
        &self,
        context: &ToolContext<'_>,
        mut report: ProcessReport,
    ) -> Result<ToolOutput, ToolError> {
        let mut artifacts = Vec::new();
        let interrupted = report.termination != Termination::Exited;
        let artifact = if interrupted || !report.success {
            json!({"status":"not_produced"})
        } else if report.stdout.truncated || report.stdout.encoding != "utf8" {
            json!({"status":"rejected", "reason":"stdout must be complete UTF-8 within the approved limit"})
        } else {
            let id = format!("artifact_{}", uuid::Uuid::new_v4().simple());
            let content = std::mem::take(&mut report.stdout.data);
            let bytes = content.len();
            artifacts.push(Artifact {
                id: id.clone(),
                task_id: context.task_id.into(),
                title: self.artifact_title.clone(),
                media_type: TEXT_MEDIA_TYPE.into(),
                body: ArtifactBody::Text { content },
            });
            json!({"status":"created","id":id,"contentBytes":bytes,"mediaType":TEXT_MEDIA_TYPE})
        };
        let mut process = serde_json::to_value(report)
            .map_err(|_| ToolError::OutcomeUnknown("cannot encode Skill result".into()))?;
        if !artifacts.is_empty() {
            // Actual content was committed to the artifact body, not duplicated into model history.
            process["stdout"].as_object_mut().unwrap().remove("data");
        }
        let output = json!({ "process":process, "artifact":artifact });
        if interrupted {
            Err(ToolError::OutcomeUnknownWithOutput {message:"Skill stopped without a complete result; inspect evidence before starting a new task".into(),output})
        } else {
            Ok(ToolOutput { output, artifacts })
        }
    }
}
