//! Model wire formats are isolated here; the kernel knows no HTTP endpoints.
mod anthropic;
mod http;
mod openai;

pub use anthropic::AnthropicProvider;
use fliuno_core::{DemoProvider, ModelProvider};
pub use openai::OpenAiProvider;

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
    let provider: Box<dyn ModelProvider> = match name.as_str() {
        "openai" => Box::new(OpenAiProvider::new(
            &std::env::var("OPENAI_BASE_URL")
                .unwrap_or_else(|_| "https://api.openai.com/v1/".into()),
            &std::env::var("OPENAI_API_KEY").map_err(|_| "OPENAI_API_KEY is required")?,
            &model,
        )?),
        "anthropic" => Box::new(AnthropicProvider::new(
            &std::env::var("ANTHROPIC_BASE_URL")
                .unwrap_or_else(|_| "https://api.anthropic.com/v1/".into()),
            &std::env::var("ANTHROPIC_API_KEY").map_err(|_| "ANTHROPIC_API_KEY is required")?,
            &model,
        )?),
        _ => return Err(format!("unknown provider: {name}")),
    };
    Ok((name, provider))
}
