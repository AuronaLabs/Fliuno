//! Model wire formats are isolated here; the kernel knows no HTTP endpoints.
mod anthropic;
mod catalog;
mod gemini;
mod http;
mod openai;
mod openai_chat;

pub use anthropic::AnthropicProvider;
pub use catalog::{check_model_connection, discover_models, preview_model_catalog};
use fliuno_core::{DemoProvider, ModelProvider};
use fliuno_protocol::{
    ModelBudget, ModelBudgetSource, ModelInputCountCoverage, ModelInputCountMode,
};
pub use gemini::GeminiProvider;
pub use openai::OpenAiProvider;

/// Opt-in guard with a host-selected context window and output reserve.
#[derive(Debug, Clone, Copy)]
pub struct ModelContextBudget {
    input_allowance: u32,
    max_output_tokens: u32,
    provider_count: bool,
    count_coverage: ModelInputCountCoverage,
    safety_margin_tokens: u32,
    source: ModelBudgetSource,
}

impl ModelContextBudget {
    fn describe(self) -> ModelBudget {
        ModelBudget {
            window_tokens: self.input_allowance + self.max_output_tokens,
            max_input_tokens: self.provider_count.then_some(self.input_allowance),
            max_input_bytes: (!self.provider_count).then_some(self.input_allowance),
            max_output_tokens: self.max_output_tokens,
            source: self.source,
            count_mode: if self.provider_count {
                ModelInputCountMode::Provider
            } else {
                ModelInputCountMode::JsonBytesEstimate
            },
            count_coverage: self.count_coverage,
            safety_margin_tokens: self.safety_margin_tokens,
        }
    }

    pub fn new(window_tokens: u32, max_output_tokens: u32) -> Result<Self, String> {
        if max_output_tokens < 16 || max_output_tokens >= window_tokens {
            return Err("model context budget needs an output limit of at least 16 and smaller than the context window".into());
        }
        Ok(Self {
            input_allowance: window_tokens - max_output_tokens,
            max_output_tokens,
            provider_count: false,
            count_coverage: ModelInputCountCoverage::SerializedJsonBytes,
            safety_margin_tokens: 0,
            source: ModelBudgetSource::Configured,
        })
    }

    /// Ask the configured provider to count the input before generating.
    pub fn with_provider_count(mut self) -> Self {
        self.provider_count = true;
        self.count_coverage = ModelInputCountCoverage::ProviderRequest;
        self
    }

    fn with_provider_projection(mut self, safety_margin_tokens: u32) -> Self {
        self.provider_count = true;
        self.count_coverage = ModelInputCountCoverage::ProviderProjection;
        self.safety_margin_tokens = safety_margin_tokens;
        self
    }
}

enum BudgetWindow {
    Tokens(u32),
    Provider,
}

struct BudgetConfig {
    window: BudgetWindow,
    max_output_tokens: u32,
    provider_count: bool,
}

impl BudgetConfig {
    fn resolve(
        self,
        discover: impl FnOnce(u32) -> Result<ModelContextBudget, String>,
    ) -> Result<ModelContextBudget, String> {
        let budget = match self.window {
            BudgetWindow::Tokens(window) => {
                ModelContextBudget::new(window, self.max_output_tokens)?
            }
            BudgetWindow::Provider => discover(self.max_output_tokens)?,
        };
        Ok(if self.provider_count {
            budget.with_provider_count()
        } else {
            budget
        })
    }
}

fn context_budget_from_env() -> Result<Option<BudgetConfig>, String> {
    fn read(name: &str) -> Result<Option<String>, String> {
        match std::env::var(name) {
            Ok(value) => Ok(Some(value)),
            Err(std::env::VarError::NotPresent) => Ok(None),
            Err(std::env::VarError::NotUnicode(_)) => {
                Err(format!("{name} must contain valid UTF-8"))
            }
        }
    }
    let provider_count = match std::env::var("FLIUNO_CONTEXT_COUNT_MODE") {
        Ok(value) if value == "provider" => true,
        Ok(value) if value == "local" => false,
        Err(std::env::VarError::NotPresent) => false,
        _ => return Err("FLIUNO_CONTEXT_COUNT_MODE must be local or provider".into()),
    };
    match (
        read("FLIUNO_CONTEXT_WINDOW_TOKENS")?,
        read("FLIUNO_MAX_OUTPUT_TOKENS")?,
    ) {
        (None, None) if !provider_count => Ok(None),
        (Some(window), Some(output)) => {
            let window = if window == "provider" {
                BudgetWindow::Provider
            } else {
                BudgetWindow::Tokens(window.parse().map_err(
                    |_| "FLIUNO_CONTEXT_WINDOW_TOKENS must be an unsigned integer or provider",
                )?)
            };
            let max_output_tokens = output
                .parse()
                .map_err(|_| "FLIUNO_MAX_OUTPUT_TOKENS must be an unsigned integer")?;
            Ok(Some(BudgetConfig {
                window,
                max_output_tokens,
                provider_count,
            }))
        }
        _ => Err("set both FLIUNO_CONTEXT_WINDOW_TOKENS and FLIUNO_MAX_OUTPUT_TOKENS".into()),
    }
}

/// No implicit paid model. Live execution requires explicit provider + model.
pub fn from_env() -> Result<(String, Box<dyn ModelProvider>), String> {
    use fliuno_config::Provider;
    let name = std::env::var("FLIUNO_PROVIDER").unwrap_or_else(|_| "demo".into());
    let (provider, url_var, key_var) = match name.as_str() {
        "demo" => return Ok((name, Box::new(DemoProvider))),
        "openai" => (Provider::OpenAi, "OPENAI_BASE_URL", "OPENAI_API_KEY"),
        "anthropic" => (
            Provider::Anthropic,
            "ANTHROPIC_BASE_URL",
            "ANTHROPIC_API_KEY",
        ),
        "gemini" => (Provider::Gemini, "GEMINI_BASE_URL", "GEMINI_API_KEY"),
        _ => return Err(format!("unknown provider: {name}")),
    };
    let model =
        std::env::var("FLIUNO_MODEL").map_err(|_| "FLIUNO_MODEL is required for live providers")?;
    let budget = context_budget_from_env()?;
    let key = std::env::var(key_var).map_err(|_| format!("{key_var} is required"))?;
    let base_url = std::env::var(url_var).ok();
    build_provider(provider, &model, base_url.as_deref(), &key, budget, None)
}

/// Validate a startup profile locally without generation or remote metadata discovery.
pub fn validate_live_profile(settings: &fliuno_config::ModelSettings) -> Result<(), String> {
    use fliuno_config::{ContextWindow, Provider};
    if matches!(settings.provider, Provider::Demo) {
        return Err("Choose a configured live model".into());
    }
    settings.effective_protocol()?;
    settings.validate_reasoning_effort()?;
    settings.validate_budget()?;
    let model = settings
        .name
        .as_deref()
        .filter(|model| !model.trim().is_empty())
        .ok_or("[model] name is required for a live provider")?;
    fliuno_config::provider_api_root(settings.provider.api_root(settings.base_url.as_deref()))?;
    match settings.provider {
        Provider::Gemini => {
            http::count_route_model_id(model)?;
        }
        Provider::Anthropic
            if settings
                .budget
                .as_ref()
                .is_some_and(|b| matches!(b.window, ContextWindow::Discovery(_))) =>
        {
            anthropic::validate_metadata_model(model)?;
        }
        _ => {}
    }
    if let Some(key) = settings.read_model_key()? {
        http::provider_headers(settings.provider, &key)?;
    }
    Ok(())
}

pub fn from_settings(
    settings: &fliuno_config::ModelSettings,
) -> Result<(String, Box<dyn ModelProvider>), String> {
    use fliuno_config::{ContextWindow, ModelProtocol, Provider};
    let protocol = settings.effective_protocol()?;
    settings.validate_reasoning_effort()?;
    settings.validate_model_auth()?;
    if matches!(settings.provider, Provider::Demo) {
        return Ok(("demo".into(), Box::new(DemoProvider)));
    }
    settings.validate_budget()?;
    let model = settings
        .name
        .as_deref()
        .ok_or("[model] name is required for a live provider")?;
    let key = settings.read_model_key()?;
    let budget = settings.budget.as_ref().map(|budget| BudgetConfig {
        window: match budget.window {
            ContextWindow::Tokens(window) => BudgetWindow::Tokens(window),
            ContextWindow::Discovery(_) => BudgetWindow::Provider,
        },
        max_output_tokens: budget.max_output_tokens,
        provider_count: budget.provider_count,
    });
    if protocol == Some(ModelProtocol::ChatCompletions) {
        let provider = openai_chat::ChatProvider::new(
            settings.provider.api_root(settings.base_url.as_deref()),
            key.as_deref(),
            model,
            settings.reasoning_effort,
            budget,
        )?;
        return Ok((settings.runtime_provider_name().into(), Box::new(provider)));
    }
    build_provider(
        settings.provider,
        model,
        settings.base_url.as_deref(),
        key.as_deref()
            .ok_or("This model protocol requires an API key")?,
        budget,
        settings.reasoning_effort,
    )
}

fn build_provider(
    kind: fliuno_config::Provider,
    model: &str,
    base_url: Option<&str>,
    key: &str,
    budget: Option<BudgetConfig>,
    effort: Option<fliuno_config::ReasoningEffort>,
) -> Result<(String, Box<dyn ModelProvider>), String> {
    use fliuno_config::Provider;
    if model.trim().is_empty() {
        return Err("The model name must not be empty".into());
    }
    let base = kind.api_root(base_url);
    let provider: Box<dyn ModelProvider> = match kind {
        Provider::Demo => Box::new(DemoProvider),
        Provider::OpenAi => {
            let provider = OpenAiProvider::new(base, key, model)?.with_effort(effort);
            let budget = budget
                .map(|config| {
                    config.resolve(|_| Err("Context window discovery is unsupported for OpenAI; configure a numeric window".into()))
                })
                .transpose()?;
            Box::new(if let Some(budget) = budget {
                provider.with_context_budget(budget)
            } else {
                provider
            })
        }
        Provider::Anthropic => {
            let mut provider = AnthropicProvider::new(base, key, model)?.with_effort(effort);
            let budget = budget
                .map(|config| config.resolve(|output| provider.discover_context_budget(output)))
                .transpose()?;
            Box::new(if let Some(budget) = budget {
                provider.with_context_budget(budget)
            } else {
                provider
            })
        }
        Provider::Gemini => {
            let provider = GeminiProvider::new(base, key, model)?.with_effort(effort);
            if let Some(config) = budget {
                let budget =
                    config.resolve(|_| Err("Context window discovery is unsupported for Gemini; configure a numeric window".into()))?;
                Box::new(provider.with_context_budget(budget)?)
            } else {
                Box::new(provider)
            }
        }
    };
    Ok((kind.name().into(), provider))
}
