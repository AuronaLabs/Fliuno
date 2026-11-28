//! Host authority is immutable per registration. Models supply only argv/stdin;
//! they cannot select another executable, environment, cwd or isolation mode.
use fliuno_sandbox::ResourceLimits;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, path::PathBuf};

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ProcessIsolation {
    TrustedHost,
    WindowsAppContainer,
    LinuxLandlock,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProgramConfig {
    #[serde(default)]
    pub assets: Vec<fliuno_sandbox::StagedFile>,
    pub id: String,
    pub command: PathBuf,
    pub isolation: ProcessIsolation,
    #[serde(default)]
    pub resources: ResourceLimits,
    /// Optional delegated cgroup v2 parent used by the Linux sandbox.
    #[serde(default)]
    pub linux_cgroup_parent: Option<PathBuf>,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env_allow: Vec<String>,
    #[serde(default = "default_timeout")]
    pub timeout_ms: u64,
    #[serde(default = "default_output")]
    pub output_bytes: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skill: Option<crate::skill::SkillExecution>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_check: Option<crate::ProjectCheckConfig>,
}

pub(crate) const MAX_OUTPUT: usize = 64 * 1024;
pub(crate) const MAX_INPUT: usize = 256 * 1024;
fn default_timeout() -> u64 {
    30_000
}
fn default_output() -> usize {
    32 * 1024
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub programs: Vec<ProgramConfig>,
}

pub(crate) struct Program {
    pub config: ProgramConfig,
    pub environment: BTreeMap<String, String>,
    pub executable_digest: String,
    pub tool_name: String,
    pub supervisor: Option<PathBuf>,
}

pub(crate) fn validate_args(args: &[String]) -> Result<(), String> {
    if args.len() > 256
        || args.iter().any(|arg| arg.contains('\0'))
        || args.iter().map(String::len).sum::<usize>() > 32_000
    {
        return Err("argv exceeds 256 arguments / 32000 UTF-8 bytes or contains NUL".into());
    }
    Ok(())
}

pub(crate) fn executable_digest(path: &std::path::Path) -> Result<String, String> {
    fliuno_sandbox::program_digest(path).map_err(|_| "cannot hash configured executable".into())
}

impl Program {
    pub fn resolve(config: ProgramConfig) -> Result<Self, String> {
        fliuno_sandbox::validate_staged_files(&config.assets)
            .map_err(|_| "invalid staged process files")?;
        if config.id.is_empty()
            || config.id.len() > 23
            || !config
                .id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
        {
            return Err("program id must be 1..23 ASCII letters, digits, '_' or '-'".into());
        }
        if !config.command.is_absolute() || !config.command.is_file() {
            return Err("program command must be an absolute existing executable path".into());
        }
        if cfg!(windows)
            && !config
                .command
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("exe"))
        {
            return Err(
                "Windows programs must be native .exe paths; configure an interpreter explicitly"
                    .into(),
            );
        }
        match config.isolation {
            ProcessIsolation::WindowsAppContainer if !cfg!(windows) => {
                return Err(
                    "windows_app_container is unsupported on this host; no host fallback".into(),
                );
            }
            ProcessIsolation::LinuxLandlock if !cfg!(target_os = "linux") => {
                return Err("linux_landlock is supported only on Linux; no host fallback".into());
            }
            _ => {}
        }
        config
            .resources
            .validate()
            .map_err(|_| "invalid or unsupported process resource limits")?;
        if config.isolation == ProcessIsolation::TrustedHost && !config.assets.is_empty() {
            return Err("staged assets require an isolated process mode".into());
        }
        if let Some(parent) = &config.linux_cgroup_parent
            && (!cfg!(target_os = "linux") || !parent.is_absolute() || !parent.is_dir())
        {
            return Err("linuxCgroupParent must be an existing Linux cgroup directory".into());
        }
        validate_args(&config.args)?;
        crate::skill::validate(&config)?;
        crate::project_check::validate(&config)?;
        if !(100..=300_000).contains(&config.timeout_ms)
            || !(1024..=MAX_OUTPUT).contains(&config.output_bytes)
        {
            return Err("program timeoutMs must be 100..300000 and outputBytes 1024..65536".into());
        }
        let mut names = std::collections::BTreeSet::new();
        if config.env_allow.len() > 64
            || config.env_allow.iter().any(|name| {
                name.is_empty()
                    || !name.is_ascii()
                    || name.contains(['=', '\0'])
                    || !names.insert(name.to_ascii_uppercase())
            })
        {
            return Err("invalid or duplicate program envAllow".into());
        }
        let mut environment = BTreeMap::new();
        for key in &config.env_allow {
            match std::env::var(key) {
                Ok(value) => {
                    environment.insert(key.clone(), value);
                }
                Err(std::env::VarError::NotPresent) => {}
                Err(_) => return Err("configured environment must be Unicode".into()),
            }
        }
        if cfg!(windows)
            && !environment
                .keys()
                .any(|name| name.eq_ignore_ascii_case("SystemRoot"))
        {
            let value = std::env::var("SystemRoot")
                .map_err(|_| "program SystemRoot must be available and Unicode at registration")?;
            environment.insert("SystemRoot".into(), value);
        }
        if environment
            .iter()
            .map(|(key, value)| key.len() + value.len())
            .sum::<usize>()
            > MAX_INPUT
        {
            return Err("program environment exceeds 256 KiB".into());
        }
        let executable_digest = executable_digest(&config.command)?;
        Self::from_parts(config, environment, executable_digest)
    }

    pub(crate) fn from_parts(
        config: ProgramConfig,
        environment: BTreeMap<String, String>,
        executable_digest: String,
    ) -> Result<Self, String> {
        // Hash actual resolved environment as well as policy and executable
        // contents. A restart with changed authority cannot rebind an old tool.
        let identity = serde_json::to_vec(&(&config, &environment, &executable_digest))
            .map_err(|_| "cannot encode program identity")?;
        let digest = format!("{:x}", Sha256::digest(identity));
        let prefix = if config.skill.is_some() {
            "skill"
        } else {
            "process"
        };
        let tool_name = format!("{prefix}_{}_{}", config.id, &digest[..32]);
        let program = Self {
            config,
            environment,
            executable_digest,
            tool_name,
            supervisor: None,
        };
        if let Some(skill) = &program.config.skill {
            skill.validate_approval_plan(&program)?;
        }
        if let Some(check) = &program.config.project_check {
            check.validate_approval_plan(&program)?;
        }
        Ok(program)
    }
}
