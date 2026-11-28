//! Approval-bound process tools. Configuration, IO capture and process ownership
//! stay separate; core retains the durable execution identity and recovery rules.
mod capture;
mod config;
#[cfg(feature = "test-fixtures")]
pub mod fixture;
mod project_check;
mod runner;
mod skill;
pub mod supervisor;
pub use capture::CapturedOutput;
pub use config::{Config, ProcessIsolation, ProgramConfig};
pub use project_check::ProjectCheckConfig;
pub use runner::{
    ProcessEvidence, ProcessFault, ProcessOperation, ProcessReport, ProcessStopCause, Termination,
};
pub use skill::SkillExecution;

use config::{MAX_INPUT, Program, validate_args};
use fliuno_core::{
    CancellationToken, Kernel, Tool, ToolContext, ToolEffect, ToolError, ToolOutput, ToolSpec,
};
use fliuno_protocol::ToolCall;
use serde::Deserialize;
use serde_json::{Value, json};

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Arguments {
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    stdin: String,
    timeout_ms: Option<u64>,
    output_bytes: Option<usize>,
    inputs: Option<std::collections::BTreeMap<String, String>>,
}

impl Program {
    fn arguments(&self, value: &Value) -> Result<Arguments, String> {
        let args: Arguments =
            serde_json::from_value(value.clone()).map_err(|_| "invalid process arguments")?;
        if let Some(check) = &self.config.project_check {
            if !value
                .as_object()
                .is_some_and(|object| object.len() == 1 && object.contains_key("inputs"))
            {
                return Err("project checks accept only the configured input hashes".into());
            }
            check.validate_hashes(
                args.inputs
                    .as_ref()
                    .ok_or("project check inputs are required")?,
            )?;
        } else if value.get("inputs").is_some() {
            return Err("only project checks take workspace input hashes".into());
        }
        if self.config.skill.is_some() {
            skill::validate_input(value, &args.stdin)?;
        }
        let combined = self
            .config
            .args
            .iter()
            .chain(&args.args)
            .cloned()
            .collect::<Vec<_>>();
        validate_args(&combined)?;
        if args.stdin.len() > MAX_INPUT {
            return Err("process stdin exceeds 256 KiB".into());
        }
        if args
            .timeout_ms
            .is_some_and(|value| !(100..=self.config.timeout_ms).contains(&value))
            || args
                .output_bytes
                .is_some_and(|value| !(1024..=self.config.output_bytes).contains(&value))
        {
            return Err("process limits must stay within the configured program budget".into());
        }
        Ok(args)
    }
}

impl Tool for Program {
    fn supports_supervision(&self) -> bool {
        cfg!(any(windows, target_os = "linux")) && self.supervisor.is_some()
    }

    fn launch_supervisor(&self, ticket: &fliuno_core::SupervisorTicket) -> fliuno_core::Result<()> {
        supervisor::launch(self, ticket)
    }

    fn spec(&self) -> ToolSpec {
        if self.config.skill.is_some() {
            return skill::spec(self);
        }
        if let Some(check) = &self.config.project_check {
            return check.spec(self);
        }
        ToolSpec {
            name: self.tool_name.clone(),
            description: format!(
                "Run the host-registered program '{}' with {:?} authority and {} fixed arguments. Dynamic argv is passed directly without a shell, stdin is bounded UTF-8, and stdout/stderr are captured as bounded UTF-8 or base64 tails. The command timeout and process-group/Job cleanup apply to the entire run. The model cannot change the executable, cwd, environment or isolation mode.",
                self.config.id,
                self.config.isolation,
                self.config.args.len()
            ),
            effect: ToolEffect::ProcessExecution,
            input_schema: json!({"type":"object","additionalProperties":false,"properties":{
                "args":{"type":"array","maxItems":256,"items":{"type":"string"}},
                "stdin":{"type":"string","description":"UTF-8 stdin, at most 256 KiB. Closed after writing."},
                "timeoutMs":{"type":"integer","minimum":100,"maximum":self.config.timeout_ms},
                "outputBytes":{"type":"integer","minimum":1024,"maximum":self.config.output_bytes,"description":"Raw bytes retained per stdout/stderr tail."}
            }}),
        }
    }
    fn validate(&self, arguments: &Value) -> Result<(), String> {
        self.arguments(arguments).map(|_| ())
    }
    fn execution_plan(&self, arguments: &Value) -> Option<Value> {
        if let Some(check) = &self.config.project_check {
            let args = self.arguments(arguments).ok()?;
            return Some(check.plan(self, &args.inputs?));
        }
        self.config
            .skill
            .as_ref()
            .map(|skill| skill.plan(self, arguments["stdin"].as_str().map_or(0, str::len)))
    }
    fn execute(
        &self,
        context: &ToolContext<'_>,
        call: &ToolCall,
        cancellation: &CancellationToken,
    ) -> Result<ToolOutput, ToolError> {
        let args = self.arguments(&call.arguments)?;
        if cancellation.is_cancelled() {
            return Err(ToolError::Failed("process cancelled before launch".into()));
        }
        if config::executable_digest(&self.config.command)? != self.executable_digest {
            return Err(ToolError::Failed(
                "configured executable changed; restart and obtain new approval".into(),
            ));
        }
        if cancellation.is_cancelled() {
            return Err(ToolError::Failed("process cancelled before launch".into()));
        }
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|_| ToolError::Failed("cannot initialize process runtime".into()))?;
        let report = runtime.block_on(runner::run(self, context, args, cancellation))?;
        if let Some(skill) = &self.config.skill {
            return skill.finish(context, report);
        }
        let interrupted = report.termination != Termination::Exited;
        let output = serde_json::to_value(report)
            .map_err(|_| ToolError::OutcomeUnknown("cannot encode process result".into()))?;
        if interrupted {
            Err(ToolError::OutcomeUnknownWithOutput{message:"process stopped before a known complete result; inspect output and effects before creating another task".into(),output})
        } else {
            Ok(ToolOutput {
                output,
                artifacts: vec![],
            })
        }
    }
}

/// Validate all presets before mutating the tool registry. Credentials remain
/// inside each immutable runtime, never in RPC arguments or persistent history.
pub fn register_config(kernel: &mut Kernel, config: Config) -> Result<(), String> {
    register(kernel, config, None)
}

pub fn register_supervised_config(
    kernel: &mut Kernel,
    config: Config,
    worker: std::path::PathBuf,
) -> Result<(), String> {
    register(kernel, config, Some(worker))
}

fn register(
    kernel: &mut Kernel,
    config: Config,
    supervisor: Option<std::path::PathBuf>,
) -> Result<(), String> {
    if config.programs.is_empty() || config.programs.len() > 32 {
        return Err("execution config must contain 1..32 programs".into());
    }
    let mut ids = std::collections::BTreeSet::new();
    let mut programs = Vec::new();
    let existing = kernel.tool_specs();
    for config in config.programs {
        if !ids.insert(config.id.clone()) {
            return Err("duplicate process program id".into());
        }
        let mut program = Program::resolve(config)?;
        program.supervisor = supervisor.clone();
        if existing.iter().any(|spec| spec.name == program.tool_name) {
            return Err("process tool is already registered".into());
        }
        programs.push(program);
    }
    kernel
        .register_tools(
            programs
                .into_iter()
                .map(|program| Box::new(program) as Box<dyn Tool>)
                .collect(),
        )
        .map_err(|error| error.to_string())
}
