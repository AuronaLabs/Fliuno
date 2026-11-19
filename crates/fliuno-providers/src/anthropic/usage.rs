//! Messages streaming updates are cumulative. Keep the latest reported count
//! for each field so input counters from message_start survive message_delta.
use fliuno_protocol::ModelUsage;
use serde_json::Value;

#[derive(Default)]
pub(super) struct Usage {
    input: Option<u64>,
    cache_write: Option<u64>,
    cache_read: Option<u64>,
    output: Option<u64>,
    reasoning: Option<u64>,
}

impl Usage {
    pub(super) fn observe_input(&mut self, usage: &Value) {
        observe(&mut self.input, &usage["input_tokens"]);
        observe(&mut self.cache_write, &usage["cache_creation_input_tokens"]);
        observe(&mut self.cache_read, &usage["cache_read_input_tokens"]);
    }

    pub(super) fn observe_complete(&mut self, usage: &Value) {
        self.observe_input(usage);
        observe(&mut self.output, &usage["output_tokens"]);
        observe(
            &mut self.reasoning,
            &usage["output_tokens_details"]["thinking_tokens"],
        );
    }

    pub(super) fn report(self) -> Option<ModelUsage> {
        let input_tokens = self
            .input?
            .checked_add(self.cache_write.unwrap_or_default())?
            .checked_add(self.cache_read.unwrap_or_default())?;
        let output_tokens = self.output?;
        Some(ModelUsage {
            provider: "anthropic".into(),
            input_tokens,
            output_tokens,
            cached_input_tokens: self.cache_read,
            reasoning_output_tokens: self.reasoning.filter(|count| *count <= output_tokens),
        })
    }
}

fn observe(target: &mut Option<u64>, value: &Value) {
    // Null or omitted fields leave the earlier cumulative value in place.
    if !value.is_null() {
        *target = value.as_u64();
    }
}
