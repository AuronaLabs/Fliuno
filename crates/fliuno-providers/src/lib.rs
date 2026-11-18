//! Model wire formats are isolated here; the kernel knows no HTTP endpoints.
mod anthropic;
mod http;
mod openai;

pub use anthropic::AnthropicProvider;
use fliuno_core::{DemoProvider, ModelProvider};
pub use openai::OpenAiProvider;

/// Opt-in local guard. Input is estimated from the complete serialized request,
/// not from a provider tokenizer or a model-specific window lookup.
#[derive(Debug, Clone, Copy)]
pub struct ModelContextBudget {
    input_allowance: u32,
    max_output_tokens: u32,
}

impl ModelContextBudget {
    pub fn new(window_tokens: u32, max_output_tokens: u32) -> Result<Self, String> {
        if max_output_tokens < 16 || max_output_tokens >= window_tokens {
            return Err("model context budget needs an output limit of at least 16 and smaller than the context window".into());
        }
        Ok(Self {
            input_allowance: window_tokens - max_output_tokens,
            max_output_tokens,
        })
    }
}

fn context_budget_from_env() -> Result<Option<ModelContextBudget>, String> {
    fn read(name: &str) -> Result<Option<u32>, String> {
        match std::env::var(name) {
            Ok(value) => value
                .parse()
                .map(Some)
                .map_err(|_| format!("{name} must be an unsigned integer")),
            Err(std::env::VarError::NotPresent) => Ok(None),
            Err(std::env::VarError::NotUnicode(_)) => {
                Err(format!("{name} must be an unsigned integer"))
            }
        }
    }
    match (
        read("FLIUNO_CONTEXT_WINDOW_TOKENS")?,
        read("FLIUNO_MAX_OUTPUT_TOKENS")?,
    ) {
        (None, None) => Ok(None),
        (Some(window), Some(output)) => ModelContextBudget::new(window, output).map(Some),
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
            Box::new(if let Some(budget) = budget {
                provider.with_context_budget(budget)
            } else {
                provider
            })
        }
        "anthropic" => {
            let provider = AnthropicProvider::new(
                &std::env::var("ANTHROPIC_BASE_URL")
                    .unwrap_or_else(|_| "https://api.anthropic.com/v1/".into()),
                &std::env::var("ANTHROPIC_API_KEY").map_err(|_| "ANTHROPIC_API_KEY is required")?,
                &model,
            )?;
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
