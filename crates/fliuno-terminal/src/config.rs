//! Immutable host presets. Authority is hashed into tool names so an old
//! approval cannot silently rebind to changed configuration or credentials.
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Config {
    pub state_directory: PathBuf,
    pub presets: Vec<PresetConfig>,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PresetConfig {
    pub id: String,
    pub command: PathBuf,
    pub isolation: Isolation,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env_allow: Vec<String>,
    #[serde(default = "lifetime")]
    pub lifetime_ms: u64,
    #[serde(default = "spool")]
    pub spool_bytes: usize,
    #[serde(default)]
    pub resources: fliuno_sandbox::ResourceLimits,
    #[serde(default)]
    pub linux_cgroup_parent: Option<PathBuf>,
}
#[derive(Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Isolation {
    TrustedHost,
}
fn lifetime() -> u64 {
    900_000
}
fn spool() -> usize {
    1024 * 1024
}

pub(crate) struct Preset {
    pub config: PresetConfig,
    pub environment: BTreeMap<String, String>,
    pub executable_digest: String,
    pub identity: String,
}
pub(crate) fn digest(path: &Path) -> Result<String, String> {
    fliuno_sandbox::program_digest(path).map_err(|_| "cannot hash terminal executable".into())
}
pub(crate) fn argv(args: &[String]) -> Result<(), String> {
    if args.len() > 256
        || args.iter().any(|s| s.contains('\0'))
        || args.iter().map(String::len).sum::<usize>() > 32000
    {
        return Err("terminal argv exceeds 256 items / 32000 bytes or contains NUL".into());
    }
    Ok(())
}
impl Preset {
    pub fn resolve(config: PresetConfig, state: &Path) -> Result<Self, String> {
        if config.id.is_empty()
            || config.id.len() > 23
            || !config
                .id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        {
            return Err("invalid terminal preset id".into());
        }
        if !config.command.is_absolute()
            || (cfg!(windows)
                && !config
                    .command
                    .extension()
                    .is_some_and(|s| s.eq_ignore_ascii_case("exe")))
        {
            return Err("terminal command must be an absolute native executable".into());
        }
        argv(&config.args)?;
        if !(1000..=86_400_000).contains(&config.lifetime_ms)
            || !(65536..=8 * 1024 * 1024).contains(&config.spool_bytes)
        {
            return Err(
                "terminal lifetimeMs must be 1000..86400000; spoolBytes 65536..8388608".into(),
            );
        }
        let mut names = BTreeSet::new();
        let mut environment = BTreeMap::new();
        if config.env_allow.len() > 64 {
            return Err("terminal envAllow exceeds 64 names".into());
        }
        for name in &config.env_allow {
            if name.is_empty()
                || !name.is_ascii()
                || name.contains(['=', '\0'])
                || !names.insert(name.to_ascii_uppercase())
            {
                return Err("invalid terminal envAllow".into());
            }
            match std::env::var(name) {
                Ok(value) if !value.contains('\0') => {
                    environment.insert(name.clone(), value);
                }
                Err(std::env::VarError::NotPresent) => {}
                _ => return Err("terminal environment must be Unicode without NUL".into()),
            }
        }
        if cfg!(windows)
            && !names.contains("SYSTEMROOT")
            && let Ok(value) = std::env::var("SystemRoot")
        {
            environment.insert("SystemRoot".into(), value);
        }
        if environment
            .iter()
            .map(|(k, v)| k.len() + v.len())
            .sum::<usize>()
            > 65536
        {
            return Err("terminal environment exceeds 64 KiB".into());
        }
        fliuno_sandbox::LaunchSpec {
            program_sha256: None,
            assets: vec![],
            program: config.command.clone(),
            args: config.args.clone(),
            env: environment.clone(),
            resources: config.resources,
            linux_cgroup_parent: config.linux_cgroup_parent.clone(),
        }
        .validate()
        .map_err(|_| "invalid terminal preset process limits or cgroup parent")?;
        let executable_digest = digest(&config.command)?;
        let identity = format!(
            "{:x}",
            Sha256::digest(
                serde_json::to_vec(&(&config, &environment, &executable_digest, state))
                    .map_err(|_| "cannot hash terminal preset")?
            )
        );
        Ok(Self {
            config,
            environment,
            executable_digest,
            identity,
        })
    }
}
