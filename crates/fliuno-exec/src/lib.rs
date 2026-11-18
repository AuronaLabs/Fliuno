//! Approval-bound process tools. Configuration, IO capture and process ownership
//! stay separate; core retains the durable execution identity and recovery rules.
mod capture;
mod config;
#[cfg(feature = "test-fixtures")]
pub mod fixture;
mod runner;
pub use capture::CapturedOutput;
pub use config::{Config, ProcessIsolation, ProgramConfig};
pub use runner::{
    ProcessEvidence, ProcessFault, ProcessOperation, ProcessReport, ProcessStopCause, Termination,
};

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
}

impl Program {
    fn arguments(&self, value: &Value) -> Result<Arguments, String> {
        let args: Arguments =
            serde_json::from_value(value.clone()).map_err(|_| "invalid process arguments")?;
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
    fn spec(&self) -> ToolSpec {
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
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|_| ToolError::Failed("cannot initialize process runtime".into()))?;
        let report = runtime.block_on(runner::run(self, context, args, cancellation))?;
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
        let program = Program::resolve(config)?;
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
