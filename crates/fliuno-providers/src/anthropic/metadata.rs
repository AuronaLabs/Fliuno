//! Optional startup budget discovery for the configured model or alias.
use super::*;
use fliuno_protocol::ModelBudgetSource;
use serde::Deserialize;

#[derive(Deserialize)]
struct Metadata {
    #[serde(rename = "type")]
    kind: String,
    id: String,
    max_input_tokens: u32,
    max_tokens: u32,
    lifecycle: Option<String>,
}

fn valid_model_id(id: &str) -> bool {
    id.len() <= 256
        && id.as_bytes().first().is_some_and(u8::is_ascii_alphanumeric)
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

pub(crate) fn validate_metadata_model(model: &str) -> Result<(), &'static str> {
    if !valid_model_id(model) {
        return Err("invalid model identifier for Anthropic metadata discovery");
    }
    Ok(())
}

impl AnthropicProvider {
    pub(crate) fn discover_context_budget(
        &mut self,
        output: u32,
    ) -> Result<ModelContextBudget, String> {
        if output < 16 {
            return Err("model budget needs an output limit of at least 16".into());
        }
        validate_metadata_model(&self.model)?;
        let metadata: Metadata = serde_json::from_value(self.http.model_metadata(&self.model)?)
            .map_err(|_| "Anthropic model metadata did not include valid limits")?;
        if metadata.kind != "model" || !valid_model_id(&metadata.id) {
            return Err("invalid Anthropic model metadata identity".into());
        }
        if metadata.lifecycle.as_deref() == Some("retired") {
            return Err("Anthropic model metadata reports a retired model".into());
        }
        if output > metadata.max_tokens {
            return Err("configured output limit exceeds the Anthropic model maximum".into());
        }
        // Apply the same conservative reservation as a numeric host budget.
        let mut budget = ModelContextBudget::new(metadata.max_input_tokens, output)?;
        budget.source = ModelBudgetSource::ProviderMetadata;
        // Resolve aliases once so count and generation use the model inspected.
        self.model = metadata.id;
        Ok(budget)
    }
}
