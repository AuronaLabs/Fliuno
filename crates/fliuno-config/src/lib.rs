//! User-owned startup settings shared by the terminal client and app-server.
mod credentials;
pub use credentials::PendingApiKey;

use serde::Deserialize;
use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    path::{Path, PathBuf},
};

pub const TEMPLATE: &str = r#"# Fliuno user settings. Paths are relative to this file.
[workbench]
theme = "auto"
# database = "state/workbench.db"

[model]
# Choose openai, anthropic, or gemini and fill in the model ID and key file.
provider = "openai"
name = ""
api_key_file = "credentials/openai.key"
"#;

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
    pub workbench: WorkbenchSettings,
    pub model: ModelSettings,
    pub models: BTreeMap<String, ModelSettings>,
    pub integrations: Integrations,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WorkbenchSettings {
    pub theme: Theme,
    pub database: Option<PathBuf>,
    pub server: Option<PathBuf>,
    pub model_profile: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Theme {
    #[default]
    Auto,
    Dark,
    Light,
    Mono,
}

#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Provider {
    #[default]
    Demo,
    OpenAi,
    Anthropic,
    Gemini,
}

impl Provider {
    pub fn name(self) -> &'static str {
        match self {
            Self::Demo => "demo",
            Self::OpenAi => "openai",
            Self::Anthropic => "anthropic",
            Self::Gemini => "gemini",
        }
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ModelSettings {
    pub provider: Provider,
    pub protocol: Option<ModelProtocol>,
    pub name: Option<String>,
    pub base_url: Option<String>,
    pub api_key_file: Option<PathBuf>,
    pub api_key_credential: Option<String>,
    pub budget: Option<BudgetSettings>,
    pub reasoning_effort: Option<ReasoningEffort>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReasoningEffort {
    None,
    Minimal,
    Low,
    Medium,
    High,
    XHigh,
    Max,
}

impl ReasoningEffort {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Minimal => "minimal",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::XHigh => "xhigh",
            Self::Max => "max",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelProtocol {
    Responses,
    ChatCompletions,
    Messages,
    Interactions,
}

impl ModelSettings {
    pub fn effective_protocol(&self) -> Result<Option<ModelProtocol>, &'static str> {
        use ModelProtocol::{ChatCompletions, Interactions, Messages, Responses};
        let default = match self.provider {
            Provider::Demo => None,
            Provider::OpenAi => Some(Responses),
            Provider::Anthropic => Some(Messages),
            Provider::Gemini => Some(Interactions),
        };
        match self.protocol {
            None => Ok(default),
            Some(ChatCompletions) if matches!(self.provider, Provider::OpenAi) => {
                Ok(Some(ChatCompletions))
            }
            Some(protocol) if Some(protocol) == default => Ok(default),
            _ => Err("The selected model protocol is incompatible with its provider"),
        }
    }

    pub fn runtime_provider_name(&self) -> &'static str {
        if self.protocol == Some(ModelProtocol::ChatCompletions)
            && matches!(self.provider, Provider::OpenAi)
        {
            "openai-chat"
        } else {
            self.provider.name()
        }
    }

    pub fn validate_reasoning_effort(&self) -> Result<(), String> {
        use ReasoningEffort::{High, Low, Max, Medium, Minimal, XHigh};
        let Some(effort) = self.reasoning_effort else {
            return Ok(());
        };
        let allowed = match self.provider {
            Provider::Demo => false,
            Provider::OpenAi => matches!(
                effort,
                ReasoningEffort::None | Minimal | Low | Medium | High | XHigh | Max
            ),
            Provider::Anthropic => matches!(effort, Low | Medium | High | XHigh | Max),
            Provider::Gemini => matches!(effort, Minimal | Low | Medium | High | XHigh),
        };
        if !allowed {
            return Err(format!(
                "{} does not accept reasoning_effort = {:?}",
                self.provider.name(),
                effort.as_str()
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BudgetSettings {
    pub window: ContextWindow,
    pub max_output_tokens: u32,
    #[serde(default)]
    pub provider_count: bool,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum ContextWindow {
    Tokens(u32),
    Discovery(Discovery),
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Discovery {
    Provider,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Integrations {
    pub mcp: Option<PathBuf>,
    pub mcp_oauth: Option<PathBuf>,
    pub mcp_oauth_vault: Option<PathBuf>,
    pub acp: Option<PathBuf>,
    pub a2a: Option<PathBuf>,
    pub execution: Option<PathBuf>,
    pub terminal: Option<PathBuf>,
    pub skills: Option<PathBuf>,
    pub memory_embedding: Option<PathBuf>,
}

/// Normalize a provider API root for generation, metadata, and setup.
pub fn provider_api_root(value: &str) -> Result<url::Url, &'static str> {
    let mut base = url::Url::parse(value).map_err(|_| "Invalid provider base URL")?;
    if !base.username().is_empty()
        || base.password().is_some()
        || base.query().is_some()
        || base.fragment().is_some()
    {
        return Err("The Base URL cannot contain credentials, a query, or a fragment");
    }
    let local = matches!(base.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"));
    if base.host_str().is_none() || !(base.scheme() == "https" || base.scheme() == "http" && local)
    {
        return Err("The Base URL requires HTTPS (HTTP is allowed on loopback)");
    }
    if !base.path().ends_with('/') {
        base.set_path(&format!("{}/", base.path()));
    }
    Ok(base)
}

pub fn user_directory() -> Option<PathBuf> {
    std::env::home_dir()
        .filter(|home| home.is_absolute())
        .map(|home| home.join(".fliuno"))
}

pub fn initialize(path: &Path) -> io::Result<()> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent)?;
    }
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    file.write_all(TEMPLATE.as_bytes())
}

impl Settings {
    pub fn load(path: &Path) -> Result<Self, String> {
        let text = read_file(path, 64 * 1024)
            .map_err(|error| format!("Cannot read configuration {}: {error}", path.display()))?;
        let mut settings: Self = toml::from_str(&text).map_err(|error| {
            format!(
                "Invalid configuration {}: {}",
                path.display(),
                error.message()
            )
        })?;
        let base = path.parent().unwrap_or_else(|| Path::new(""));
        let integrations = &mut settings.integrations;
        for path in [
            &mut settings.workbench.database,
            &mut settings.workbench.server,
            &mut integrations.mcp,
            &mut integrations.mcp_oauth,
            &mut integrations.mcp_oauth_vault,
            &mut integrations.acp,
            &mut integrations.a2a,
            &mut integrations.execution,
            &mut integrations.terminal,
            &mut integrations.skills,
            &mut integrations.memory_embedding,
        ] {
            resolve_relative(path, base);
        }
        resolve_relative(&mut settings.model.api_key_file, base);
        for (id, model) in &mut settings.models {
            validate_profile_id(id)?;
            resolve_relative(&mut model.api_key_file, base);
        }
        Ok(settings)
    }

    /// Resolve a startup override, a configured default, or the original single model.
    pub fn model_for_launch(&self, profile: Option<&str>) -> Result<&ModelSettings, String> {
        let Some(id) = profile.or(self.workbench.model_profile.as_deref()) else {
            return Ok(&self.model);
        };
        validate_profile_id(id)?;
        let model = self
            .models
            .get(id)
            .ok_or_else(|| format!("Unknown model profile {id}"))?;
        if matches!(model.provider, Provider::Demo) {
            return Err(format!("Model profile {id} requires a live provider"));
        }
        Ok(model)
    }
}

fn validate_profile_id(id: &str) -> Result<(), String> {
    if id.is_empty()
        || id.len() > 48
        || !id
            .bytes()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, b'-' | b'_'))
    {
        return Err("Model profile names use 1–48 letters, digits, hyphens or underscores".into());
    }
    Ok(())
}

fn resolve_relative(path: &mut Option<PathBuf>, base: &Path) {
    if let Some(relative) = path.as_ref().filter(|path| path.is_relative()) {
        *path = Some(base.join(relative));
    }
}

pub fn read_api_key(path: &Path) -> Result<String, &'static str> {
    let key = read_file(path, 8194).map_err(|_| "Cannot read the configured API key file")?;
    let key = key.trim_end_matches(['\r', '\n']);
    validate_api_key(key)?;
    Ok(key.to_owned())
}

fn validate_api_key(key: &str) -> Result<(), &'static str> {
    if key.is_empty()
        || key.len() > 8192
        || key.chars().any(|ch| ch.is_whitespace() || ch.is_control())
    {
        return Err("The API key file must contain one non-empty key");
    }
    Ok(())
}

fn read_file(path: &Path, limit: u64) -> io::Result<String> {
    if !path.metadata()?.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "expected a regular file",
        ));
    }
    let mut text = String::new();
    File::open(path)?
        .take(limit + 1)
        .read_to_string(&mut text)?;
    if text.len() as u64 > limit {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "file exceeds size limit",
        ));
    }
    Ok(text)
}
