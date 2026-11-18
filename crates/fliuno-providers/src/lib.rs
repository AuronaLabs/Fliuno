//! Model wire formats are isolated here; the kernel knows no HTTP endpoints.
mod anthropic;
mod http;
mod openai;

pub use anthropic::AnthropicProvider;
use fliuno_core::{DemoProvider, ModelProvider};
use fliuno_protocol::{ModelBudget, ModelBudgetSource, ModelInputCountMode};
pub use openai::OpenAiProvider;

/// Opt-in guard with a host-selected context window and output reserve.
#[derive(Debug, Clone, Copy)]
pub struct ModelContextBudget {
    input_allowance: u32,
    max_output_tokens: u32,
    provider_count: bool,
    source: ModelBudgetSource,
}

impl ModelContextBudget {
    fn describe(self) -> ModelBudget {
        ModelBudget {
            window_tokens: self.input_allowance + self.max_output_tokens,
            max_input_tokens: self.input_allowance,
            max_output_tokens: self.max_output_tokens,
            source: self.source,
            count_mode: if self.provider_count {
                ModelInputCountMode::Provider
            } else {
                ModelInputCountMode::JsonBytesEstimate
            },
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
            source: ModelBudgetSource::Configured,
        })
    }

    /// Ask the configured provider to count the input before generating.
    pub fn with_provider_count(mut self) -> Self {
        self.provider_count = true;
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
    let name = std::env::var("FLIUNO_PROVIDER").unwrap_or_else(|_| "demo".into());
    if name == "demo" {
        return Ok((name, Box::new(DemoProvider)));
    }
    let model =
        std::env::var("FLIUNO_MODEL").map_err(|_| "FLIUNO_MODEL is required for live providers")?;
    if model.trim().is_empty() {
        return Err("FLIUNO_MODEL must not be empty".into());
    }
    let budget = context_budget_from_env()?;
    let provider: Box<dyn ModelProvider> = match name.as_str() {
        "openai" => {
            let provider = OpenAiProvider::new(
                &std::env::var("OPENAI_BASE_URL")
                    .unwrap_or_else(|_| "https://api.openai.com/v1/".into()),
                &std::env::var("OPENAI_API_KEY").map_err(|_| "OPENAI_API_KEY is required")?,
                &model,
            )?;
            let budget = budget.map(|config| config.resolve(|_| {
                Err("FLIUNO_CONTEXT_WINDOW_TOKENS=provider is unsupported for OpenAI; configure a numeric window".into())
            })).transpose()?;
            Box::new(if let Some(budget) = budget {
                provider.with_context_budget(budget)
            } else {
                provider
            })
        }
        "anthropic" => {
            let mut provider = AnthropicProvider::new(
                &std::env::var("ANTHROPIC_BASE_URL")
                    .unwrap_or_else(|_| "https://api.anthropic.com/v1/".into()),
                &std::env::var("ANTHROPIC_API_KEY").map_err(|_| "ANTHROPIC_API_KEY is required")?,
                &model,
            )?;
            let budget = budget
                .map(|config| config.resolve(|output| provider.discover_context_budget(output)))
                .transpose()?;
            Box::new(if let Some(budget) = budget {
                provider.with_context_budget(budget)
            } else {
                provider
            })
        }
        _ => return Err(format!("unknown provider: {name}")),
    };
    Ok((name, provider))
}
