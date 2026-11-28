//! A user-configured check runs against a bounded snapshot in private scratch.
//! A model cannot choose a different program, argv, input path or environment.
use crate::{ProcessIsolation, Program, ProgramConfig};
use cap_fs_ext::{DirExt, FollowSymlinks, MetadataExt, OpenOptionsFollowExt};
use cap_std::{
    ambient_authority,
    fs::{Dir, OpenOptions},
};
use fliuno_core::{CancellationToken, ToolEffect, ToolError, ToolSpec};
use fliuno_protocol::MAX_APPROVAL_EXECUTION_PLAN_BYTES;
use fliuno_sandbox::{StagedFile, validate_staged_file_paths};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs,
    io::{Read, Write},
    path::Path,
};

const MAX_INPUT_FILES: usize = 16;
const MAX_INPUT_BYTES: u64 = 8 * 1024 * 1024;

/// Host-selected files captured from the workspace after approval.
#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProjectCheckConfig {
    pub label: String,
    pub description: String,
    pub inputs: Vec<String>,
}

pub(crate) fn validate(config: &ProgramConfig) -> Result<(), String> {
    let Some(check) = &config.project_check else {
        return Ok(());
    };
    if config.isolation == ProcessIsolation::TrustedHost
        || config.skill.is_some()
        || !config.env_allow.is_empty()
    {
        return Err(
            "project checks require native isolation, no Skill and no custom environment".into(),
        );
    }
    if check.inputs.is_empty() || check.inputs.len() > MAX_INPUT_FILES {
        return Err("project checks require 1..16 workspace input files".into());
    }
    if !valid_text(&check.label, 80)
        || !valid_text(&check.description, 512)
        || config
            .args
            .iter()
            .any(|arg| arg.chars().any(char::is_control))
        || !config
            .command
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| valid_text(name, 256))
    {
        return Err(
            "invalid project check label, description, executable name or fixed arguments".into(),
        );
    }
    validate_staged_file_paths(
        check
            .inputs
            .iter()
            .map(String::as_str)
            .chain(config.assets.iter().map(|asset| asset.path.as_str())),
    )
    .map_err(|error| format!("invalid project check paths: {error}"))?;
    Ok(())
}

fn valid_text(value: &str, limit: usize) -> bool {
    !value.trim().is_empty() && value.len() <= limit && !value.chars().any(char::is_control)
}

impl ProjectCheckConfig {
    pub(crate) fn validate_approval_plan(&self, program: &Program) -> Result<(), String> {
        let example = self
            .inputs
            .iter()
            .map(|path| (path.clone(), "0".repeat(64)))
            .collect();
        if self.plan(program, &example).to_string().len() > MAX_APPROVAL_EXECUTION_PLAN_BYTES {
            return Err(
                "project check approval exceeds 8 KiB; shorten fixed arguments or file paths"
                    .into(),
            );
        }
        Ok(())
    }

    pub(crate) fn validate_hashes(&self, hashes: &BTreeMap<String, String>) -> Result<(), String> {
        if hashes.len() != self.inputs.len()
            || !self.inputs.iter().all(|path| {
                hashes.get(path).is_some_and(|hash| {
                    hash.len() == 64
                        && hash
                            .bytes()
                            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
                })
            })
        {
            return Err(
                "provide lowercase SHA-256 for exactly the configured project check inputs".into(),
            );
        }
        Ok(())
    }

    pub(crate) fn spec(&self, program: &Program) -> ToolSpec {
        let properties = self
            .inputs
            .iter()
            .map(|path| {
                (
                    path.clone(),
                    json!({"type":"string","pattern":"^[0-9a-f]{64}$"}),
                )
            })
            .collect::<serde_json::Map<_, _>>();
        ToolSpec {
            name: program.tool_name.clone(),
            description: format!(
                "Run project check '{}': {}. Get the current SHA-256 for each declared input with a workspace read or hash tool. The configured check runs offline against copies of precisely these files in private scratch. It cannot write back to the project. Use the exit code and output to report what this check verified.",
                self.label, self.description
            ),
            effect: ToolEffect::ProcessExecution,
            input_schema: json!({"type":"object","additionalProperties":false,"required":["inputs"],"properties":{
                "inputs":{"type":"object","description":"Workspace paths and their exact current lowercase SHA-256 digests","required":self.inputs,"additionalProperties":false,"properties":properties}
            }}),
        }
    }

    pub(crate) fn plan(&self, program: &Program, hashes: &BTreeMap<String, String>) -> Value {
        let config = &program.config;
        json!({
            "kind":"project_check", "label":self.label, "program":config.command.file_name().and_then(|name| name.to_str()),
            "programSha256":program.executable_digest, "args":config.args,
            "cwd":"private_project_snapshot", "inputs":hashes, "inputBytesMax":MAX_INPUT_BYTES,
            "stagedAssets":config.assets.iter().map(|asset| json!({"path":asset.path,"sha256":asset.sha256,"executable":asset.executable})).collect::<Vec<_>>(),
            "isolation":config.isolation, "network":"no_network_grant", "workspaceWrites":false,
            "timeoutMs":config.timeout_ms, "outputBytes":config.output_bytes,
        })
    }
}

pub(crate) struct Snapshot {
    // Keep source files alive until the sandbox has copied and checked them.
    _temp: tempfile::TempDir,
    pub(crate) files: Vec<StagedFile>,
}

pub(crate) fn prepare(
    workspace: &Path,
    hashes: &BTreeMap<String, String>,
    cancellation: &CancellationToken,
    deadline: tokio::time::Instant,
) -> Result<Snapshot, ToolError> {
    let check_budget = || {
        if cancellation.is_cancelled() {
            Err(ToolError::Failed(
                "project check cancelled before launch".into(),
            ))
        } else if tokio::time::Instant::now() >= deadline {
            Err(ToolError::Failed(
                "project input snapshot exhausted the check time budget before launch".into(),
            ))
        } else {
            Ok(())
        }
    };
    check_budget()?;
    let root = Dir::open_ambient_dir(workspace, ambient_authority())
        .map_err(|_| "cannot open project workspace")?;
    let temp = tempfile::Builder::new()
        .prefix("fliuno-project-inputs-")
        .tempdir()
        .map_err(|_| "cannot prepare project check scratch inputs")?;
    let mut files = Vec::with_capacity(hashes.len());
    let mut bytes = 0;
    let mut buffer = [0u8; 32 * 1024];
    for (index, (path, expected)) in hashes.iter().enumerate() {
        let missing = || {
            ToolError::Failed(format!(
                "cannot snapshot project input '{path}'; only regular files without links are supported"
            ))
        };
        check_budget()?;
        let (parent, name) = path.rsplit_once('/').unwrap_or(("", path.as_str()));
        let mut dir = root.try_clone().map_err(|_| missing())?;
        if !parent.is_empty() {
            for component in parent.split('/') {
                dir = dir.open_dir_nofollow(component).map_err(|_| missing())?;
            }
        }
        let mut options = OpenOptions::new();
        options.read(true).follow(FollowSymlinks::No);
        #[cfg(unix)]
        {
            use cap_fs_ext::OpenOptionsSyncExt;
            options.nonblock(true);
        }
        let mut input = dir.open_with(name, &options).map_err(|_| missing())?;
        let metadata = input.metadata().map_err(|_| missing())?;
        if !metadata.is_file() || metadata.nlink() != 1 {
            return Err(missing());
        }
        let source = temp.path().join(index.to_string());
        let mut staged = fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&source)
            .map_err(|_| "cannot stage project check input")?;
        let mut hash = Sha256::new();
        loop {
            check_budget()?;
            let count = input.read(&mut buffer).map_err(|_| missing())?;
            if count == 0 {
                break;
            }
            bytes += count as u64;
            if bytes > MAX_INPUT_BYTES {
                return Err("project check inputs exceed 8 MiB".into());
            }
            let chunk = &buffer[..count];
            hash.update(chunk);
            staged
                .write_all(chunk)
                .map_err(|_| "cannot stage project check input")?;
        }
        let actual = format!("{:x}", hash.finalize());
        if actual != *expected {
            return Err(ToolError::NeedsCorrection {
                code: "project_check_input_changed",
                message: format!(
                    "project input '{path}' changed since review; read its current hash and request approval for a fresh check"
                ),
            });
        }
        files.push(StagedFile {
            source,
            path: path.clone(),
            sha256: actual,
            executable: false,
        });
    }
    check_budget()?;
    Ok(Snapshot { _temp: temp, files })
}
