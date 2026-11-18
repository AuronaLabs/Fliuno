//! Thin approval adapters. ToolContext supplies the authoritative session;
//! model arguments cannot choose another resource scope or host configuration.
use crate::{
    Config,
    config::{self, Preset},
    protocol::*,
    service::{CreateOptions, Service, identity, runtime},
};
use fliuno_core::{
    CancellationToken, Kernel, Tool, ToolContext, ToolEffect, ToolError, ToolOutput, ToolSpec,
};
use fliuno_protocol::ToolCall;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{path::PathBuf, sync::Arc};

#[cfg(test)]
mod tests;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Create {
    #[serde(default)]
    args: Vec<String>,
    #[serde(default = "rows")]
    rows: u16,
    #[serde(default = "cols")]
    cols: u16,
}
fn rows() -> u16 {
    24
}
fn cols() -> u16 {
    80
}
struct Start {
    service: Service,
    preset: Arc<Preset>,
    name: String,
}
impl Start {
    fn arguments(&self, value: &Value) -> Result<Create, String> {
        let a: Create = serde_json::from_value(value.clone())
            .map_err(|_| "invalid terminal create arguments")?;
        dimensions(a.rows, a.cols)?;
        config::argv(
            &self
                .preset
                .config
                .args
                .iter()
                .chain(&a.args)
                .cloned()
                .collect::<Vec<_>>(),
        )?;
        Ok(a)
    }
}
impl Tool for Start {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.name.clone(),
            description: format!(
                "Start host terminal preset '{}' in this session's workspace with trusted_host OS authority. PTY survives app-server restart until kill/release or {} ms lifetime. A single approval starts the preset; additional terminal input requires separate approval. No executable, cwd, env or isolation override.",
                self.preset.config.id, self.preset.config.lifetime_ms
            ),
            effect: ToolEffect::TerminalControl,
            input_schema: json!({"type":"object","additionalProperties":false,"properties":{"args":{"type":"array","maxItems":256,"items":{"type":"string"}},"rows":{"type":"integer","minimum":1,"maximum":500},"cols":{"type":"integer","minimum":1,"maximum":500}}}),
        }
    }
    fn validate(&self, value: &Value) -> Result<(), String> {
        self.arguments(value).map(|_| ())
    }
    fn execute(
        &self,
        c: &ToolContext<'_>,
        call: &ToolCall,
        cancel: &CancellationToken,
    ) -> Result<ToolOutput, ToolError> {
        let a = self.arguments(&call.arguments)?;
        let rt = runtime()?;
        let output = rt.block_on(self.service.create(
            &self.preset,
            c,
            &call.id,
            CreateOptions {
                args: a.args,
                rows: a.rows,
                cols: a.cols,
            },
            cancel,
        ))?;
        Ok(ToolOutput {
            output,
            artifacts: vec![],
        })
    }
}
#[derive(Clone, Copy)]
enum Kind {
    List,
    Status,
    Output,
    Write,
    Resize,
    Kill,
    Release,
}
impl Kind {
    fn name(self) -> &'static str {
        match self {
            Self::List => "list",
            Self::Status => "status",
            Self::Output => "output",
            Self::Write => "write",
            Self::Resize => "resize",
            Self::Kill => "kill",
            Self::Release => "release",
        }
    }
}
struct Control {
    kind: Kind,
    service: Service,
    name: String,
}

/// Retention is a distinct host operation, not a supervisor command. It removes
/// output evidence only after approval and never kills a live terminal.
struct Archive {
    service: Service,
    name: String,
}
impl Tool for Archive {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.name.clone(),
            description: "Archive a stopped terminal belonging to this session. Permanently discard retained output and input operation cache, preserving its identity and final snapshot. Fails while a supervisor owns it; release first and wait for shutdown. Requires separate approval.".into(),
            effect: ToolEffect::TerminalControl,
            input_schema: json!({"type":"object","additionalProperties":false,"required":["terminalId"],"properties":{"terminalId":{"type":"string","pattern":"^[a-f0-9]{64}$"}}}),
        }
    }
    fn validate(&self, value: &Value) -> Result<(), String> {
        let arguments: Id = decode(value)?;
        crate::registry::valid_id(&arguments.terminal_id)
    }
    fn execute(
        &self,
        context: &ToolContext<'_>,
        call: &ToolCall,
        cancellation: &CancellationToken,
    ) -> Result<ToolOutput, ToolError> {
        self.validate(&call.arguments)?;
        if cancellation.is_cancelled() {
            return Err("terminal archive cancelled before delivery".into());
        }
        let arguments: Id = decode(&call.arguments)?;
        let result = self
            .service
            .archive(context.session_id, &arguments.terminal_id)?;
        Ok(ToolOutput {
            output: json!(result),
            artifacts: vec![],
        })
    }
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Id {
    terminal_id: String,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct OutputArgs {
    terminal_id: String,
    #[serde(default)]
    after: u64,
    #[serde(default = "read_limit")]
    limit: usize,
}
fn read_limit() -> usize {
    READ_LIMIT
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct WriteArgs {
    terminal_id: String,
    data: String,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ResizeArgs {
    terminal_id: String,
    rows: u16,
    cols: u16,
}
fn decode<T: serde::de::DeserializeOwned>(value: &Value) -> Result<T, String> {
    serde_json::from_value(value.clone()).map_err(|_| "invalid terminal arguments".into())
}
impl Control {
    fn request(&self, value: &Value, id: String) -> Result<Option<(String, Operation)>, String> {
        let pair = match self.kind {
            Kind::List => {
                if value != &json!({}) {
                    return Err("terminal list takes no arguments".into());
                }
                return Ok(None);
            }
            Kind::Status | Kind::Kill | Kind::Release => {
                let a: Id = decode(value)?;
                let op = match self.kind {
                    Kind::Status => Operation::Status,
                    Kind::Kill => Operation::Kill { operation_id: id },
                    _ => Operation::Release { operation_id: id },
                };
                (a.terminal_id, op)
            }
            Kind::Output => {
                let a: OutputArgs = decode(value)?;
                if a.after > i64::MAX as u64 || !(1..=READ_LIMIT).contains(&a.limit) {
                    return Err("invalid terminal output budget".into());
                }
                (
                    a.terminal_id,
                    Operation::Output {
                        after: a.after,
                        limit: a.limit,
                    },
                )
            }
            Kind::Write => {
                let a: WriteArgs = decode(value)?;
                if a.data.is_empty() || a.data.len() > INPUT_LIMIT {
                    return Err("terminal input must be 1..16384 UTF-8 bytes".into());
                }
                (
                    a.terminal_id,
                    Operation::Write {
                        operation_id: id,
                        data: a.data,
                    },
                )
            }
            Kind::Resize => {
                let a: ResizeArgs = decode(value)?;
                dimensions(a.rows, a.cols)?;
                (
                    a.terminal_id,
                    Operation::Resize {
                        operation_id: id,
                        rows: a.rows,
                        cols: a.cols,
                    },
                )
            }
        };
        if pair.0.len() != 64 || !pair.0.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err("invalid terminal id".into());
        }
        Ok(Some(pair))
    }
}
impl Tool for Control {
    fn spec(&self) -> ToolSpec {
        let (required, mut properties) = match self.kind {
            Kind::List => (vec![], json!({})),
            Kind::Write => (
                vec!["terminalId", "data"],
                json!({"data":{"type":"string","maxLength":16384}}),
            ),
            Kind::Resize => (
                vec!["terminalId", "rows", "cols"],
                json!({"rows":{"type":"integer","minimum":1,"maximum":500},"cols":{"type":"integer","minimum":1,"maximum":500}}),
            ),
            Kind::Output => (
                vec!["terminalId"],
                json!({"after":{"type":"integer","minimum":0},"limit":{"type":"integer","minimum":1,"maximum":READ_LIMIT}}),
            ),
            _ => (vec!["terminalId"], json!({})),
        };
        if !matches!(self.kind, Kind::List) {
            properties["terminalId"] = json!({"type":"string","pattern":"^[a-f0-9]{64}$"});
        }
        ToolSpec {
            name: self.name.clone(),
            description: format!(
                "{} a terminal belonging to the current session. Output is a bounded base64 byte log with cursors and a gap flag; input is never automatically retried. Kill/release stops its supervisor and descendants. Terminals have trusted_host OS authority.",
                self.kind.name()
            ),
            effect: ToolEffect::TerminalControl,
            input_schema: json!({"type":"object","additionalProperties":false,"required":required,"properties":properties}),
        }
    }
    fn validate(&self, value: &Value) -> Result<(), String> {
        self.request(value, "0".repeat(64)).map(|_| ())
    }
    fn execute(
        &self,
        c: &ToolContext<'_>,
        call: &ToolCall,
        cancel: &CancellationToken,
    ) -> Result<ToolOutput, ToolError> {
        if cancel.is_cancelled() {
            return Err("terminal operation cancelled before delivery".into());
        }
        let request = self.request(
            &call.arguments,
            identity(&[c.task_id, &call.id, &self.name]),
        )?;
        let rt = runtime()?;
        let output = if let Some((id, op)) = request {
            match op {
                Operation::Status=>json!(rt.block_on(self.service.status(c.session_id,&id))?),
                Operation::Output{after,limit}=>json!(rt.block_on(self.service.output(c.session_id,&id,after,limit))?),
                op=>rt.block_on(async{tokio::select!{r=self.service.call(c.session_id,&id,op)=>r,_=cancel.cancelled()=>Err("terminal delivery interrupted; outcome unknown; never replay input".into())}}).map_err(ToolError::OutcomeUnknown)?
            }
        } else {
            json!(rt.block_on(self.service.list(c.session_id))?)
        };
        Ok(ToolOutput {
            output,
            artifacts: vec![],
        })
    }
}

/// Returns a facade for terminal app RPC. Validate all presets before tool
/// mutation; worker executable is supplied by the app, not model configuration.
pub fn register_config(
    kernel: &mut Kernel,
    config: Config,
    worker: PathBuf,
) -> Result<Service, String> {
    if config.presets.is_empty() || config.presets.len() > 16 {
        return Err("terminal config needs 1..16 presets".into());
    }
    let service = Service::new(config.state_directory, worker)?;
    let mut ids = std::collections::BTreeSet::new();
    let mut tools: Vec<Box<dyn Tool>> = vec![];
    for preset in config.presets {
        if !ids.insert(preset.id.clone()) {
            return Err("duplicate terminal preset id".into());
        }
        let preset = Arc::new(Preset::resolve(preset, &service.root)?);
        let name = format!("terminal_{}_{}", preset.config.id, &preset.identity[..24]);
        tools.push(Box::new(Start {
            service: service.clone(),
            preset,
            name,
        }));
    }
    let control_id = identity(&[service
        .root
        .to_str()
        .ok_or("terminal state path must be UTF-8")?]);
    for kind in [
        Kind::List,
        Kind::Status,
        Kind::Output,
        Kind::Write,
        Kind::Resize,
        Kind::Kill,
        Kind::Release,
    ] {
        tools.push(Box::new(Control {
            kind,
            service: service.clone(),
            name: format!("terminal_{}_{}", kind.name(), &control_id[..32]),
        }));
    }
    tools.push(Box::new(Archive {
        service: service.clone(),
        name: format!("terminal_archive_{}", &control_id[..32]),
    }));
    kernel.register_tools(tools).map_err(|e| e.to_string())?;
    Ok(service)
}
