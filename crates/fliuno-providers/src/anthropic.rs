use crate::{
    ModelContextBudget,
    http::{Http, provider_headers},
};
use fliuno_core::{
    CancellationToken, ModelDecision, ModelError, ModelProvider, ModelStreamEvent, ModelTurn,
    ProviderReplay, ToolSpec,
};
use fliuno_protocol::{Message, Task, ToolCall};
use fliuno_protocol::{PDF_MIME_TYPE, UserContentPart};
use serde_json::{Value, json};
mod metadata;
pub(crate) use metadata::validate_metadata_model;
mod stream;
mod usage;

pub struct AnthropicProvider {
    http: Http,
    model: String,
    effort: Option<fliuno_config::ReasoningEffort>,
}

impl AnthropicProvider {
    pub fn new(base: &str, key: &str, model: &str) -> Result<Self, String> {
        let headers = provider_headers(fliuno_config::Provider::Anthropic, key)?;
        Ok(Self {
            http: Http::new(
                base,
                "messages",
                "messages/count_tokens",
                &["model", "messages", "system", "tools", "tool_choice"],
                headers,
            )?,
            model: model.into(),
            effort: None,
        })
    }

    pub(crate) fn with_supplier(mut self, supplier: Option<&str>) -> Self {
        self.http = self.http.with_supplier(supplier);
        self
    }

    pub(crate) fn with_effort(mut self, effort: Option<fliuno_config::ReasoningEffort>) -> Self {
        self.effort = effort;
        self
    }

    pub fn with_context_budget(mut self, budget: ModelContextBudget) -> Self {
        self.http = self.http.with_context_budget(budget);
        self
    }

    fn request(
        &self,
        task: &Task,
        tools: &[ToolSpec],
        streaming: bool,
    ) -> Result<Value, ModelError> {
        let mut body =
            request(&self.model, task, tools, streaming).map_err(ModelError::NotDispatched)?;
        if let Some(output) = self.http.max_output_tokens() {
            body["max_tokens"] = json!(output);
        }
        if let Some(effort) = self.effort {
            body["output_config"]["effort"] = json!(effort.as_str());
        }
        Ok(body)
    }
}

fn request(model: &str, task: &Task, tools: &[ToolSpec], streaming: bool) -> Result<Value, String> {
    let mut messages = Vec::new();
    if let Some(history) = fliuno_core::task_context_prompt(task).map_err(|e| e.to_string())? {
        messages.push(json!({"role":"user","content":history}));
    }
    for message in &task.messages {
        match message {
            Message::User { text } => messages.push(json!({"role":"user","content":text})),
            Message::UserParts { parts } => messages.push(json!({"role":"user", "content":parts.iter().map(|part| match part {
                UserContentPart::Text { text } => json!({"type":"text", "text":text}),
                UserContentPart::Image { mime_type, data } => json!({"type":"image", "source":{"type":"base64", "media_type":mime_type.as_str(), "data":data}}),
                UserContentPart::Pdf { data } => json!({"type":"document", "source":{"type":"base64", "media_type":PDF_MIME_TYPE, "data":data}}),
            }).collect::<Vec<_>>()})),
            Message::ProviderReplay { provider, data } if provider == "anthropic" => {
                messages.push(json!({"role":"assistant","content":data}));
            }
            Message::ProviderReplay { .. } => {
                return Err("cannot switch providers inside a task".into());
            }
            Message::ToolResult { call_id, output } => {
                messages.push(json!({"role":"user", "content":[{
                    "type":"tool_result","tool_use_id":call_id,"content":output.to_string()
                }]}))
            }
            Message::ToolCall { .. } | Message::Assistant { .. } => {}
        }
    }
    Ok(
        json!({"model":model,"max_tokens":8192,"messages":messages,"stream":streaming,
            "system":fliuno_core::agent_instructions(),
            "tool_choice":{"type":"auto","disable_parallel_tool_use":true},
            "tools":tools.iter().map(|tool| json!({"name":tool.name, "description":tool.description,"input_schema":tool.input_schema})).collect::<Vec<_>>()
        }),
    )
}

fn parse(response: Value) -> Result<ModelTurn, String> {
    let reason = response["stop_reason"]
        .as_str()
        .ok_or("missing Anthropic stop_reason")?;
    if !matches!(reason, "end_turn" | "tool_use") {
        return Err(format!("Anthropic response stopped with {reason}"));
    }
    let content = response["content"]
        .as_array()
        .ok_or("missing Anthropic content")?;
    let calls: Vec<_> = content.iter().filter(|v| v["type"] == "tool_use").collect();
    if calls.len() > 1 {
        return Err("provider returned parallel calls in serial mode".into());
    }
    let decision = if let Some(call) = calls.first() {
        if reason != "tool_use" {
            return Err("inconsistent Anthropic tool stop reason".into());
        }
        ModelDecision::Call(ToolCall {
            id: call["id"].as_str().ok_or("missing tool use ID")?.into(),
            name: call["name"].as_str().ok_or("missing tool name")?.into(),
            arguments: call.get("input").cloned().ok_or("missing tool input")?,
        })
    } else {
        if reason == "tool_use" {
            return Err("tool_use stop without tool call".into());
        }
        let text: String = content
            .iter()
            .filter(|v| v["type"] == "text")
            .filter_map(|v| v["text"].as_str())
            .collect();
        if text.is_empty() {
            return Err("Anthropic returned neither tool use nor text".into());
        }
        ModelDecision::Complete(text)
    };
    let mut usage = usage::Usage::default();
    usage.observe_complete(&response["usage"]);
    Ok(ModelTurn {
        decision,
        usage: usage.report(),
        preflight: None,
        replay: Some(ProviderReplay {
            provider: "anthropic".into(),
            data: Value::Array(content.clone()),
        }),
    })
}

impl ModelProvider for AnthropicProvider {
    fn configured_budget(&self) -> Option<fliuno_protocol::ModelBudget> {
        self.http.configured_budget()
    }

    fn reasoning_effort(&self) -> Option<&'static str> {
        self.effort.map(fliuno_config::ReasoningEffort::as_str)
    }

    fn reply_identity(&self) -> Option<fliuno_protocol::ReplyIdentity> {
        Some(self.http.reply_identity("anthropic", &self.model))
    }

    fn model_identity(&self) -> Option<(String, String)> {
        Some(("anthropic".into(), self.model.clone()))
    }

    fn model_identity_source(&self) -> fliuno_protocol::ModelCapabilitiesSource {
        fliuno_protocol::ModelCapabilitiesSource::ProviderMetadata
    }

    fn supported_input_types(&self) -> Vec<fliuno_protocol::ModelInputType> {
        use fliuno_protocol::ModelInputType::{Image, Pdf, Text};
        vec![Text, Image, Pdf]
    }

    fn set_calibrated_safety_margin(&self, provider: &str, model: &str, tokens: u32) {
        if provider == "anthropic" && model == self.model {
            self.http.calibrate_safety_margin(tokens);
        }
    }

    fn observe_preflight(
        &self,
        preflight: &fliuno_protocol::ModelInputPreflight,
        actual_input_tokens: u64,
    ) {
        if preflight.provider == "anthropic" && preflight.model == self.model {
            self.http
                .observe_preflight(preflight.estimated_input_tokens, actual_input_tokens);
        }
    }

    fn supports_progress(&self) -> bool {
        true
    }

    fn supports_context_compaction(&self) -> bool {
        true
    }

    fn next(
        &self,
        task: &Task,
        tools: &[ToolSpec],
        cancellation: &CancellationToken,
    ) -> Result<ModelTurn, String> {
        self.next_classified(task, tools, cancellation)
            .map_err(|error| error.to_string())
    }

    fn next_classified(
        &self,
        task: &Task,
        tools: &[ToolSpec],
        cancellation: &CancellationToken,
    ) -> Result<ModelTurn, ModelError> {
        if cancellation.is_cancelled() {
            return Err(ModelError::CancelledBeforeDispatch);
        }
        let body = self.request(task, tools, false)?;
        if cancellation.is_cancelled() {
            return Err(ModelError::CancelledBeforeDispatch);
        }
        let (response, estimated_input_tokens) = self.http.post_observed(&body, cancellation)?;
        let mut turn = parse(response).map_err(ModelError::OutcomeUnknown)?;
        turn.preflight =
            self.http
                .preflight_report("anthropic", &self.model, estimated_input_tokens);
        Ok(turn)
    }

    fn next_with_progress(
        &self,
        task: &Task,
        tools: &[ToolSpec],
        cancellation: &CancellationToken,
        progress: &mut dyn FnMut(ModelStreamEvent),
    ) -> Result<ModelTurn, String> {
        self.next_with_progress_classified(task, tools, cancellation, progress)
            .map_err(|error| error.to_string())
    }

    fn next_with_progress_classified(
        &self,
        task: &Task,
        tools: &[ToolSpec],
        cancellation: &CancellationToken,
        progress: &mut dyn FnMut(ModelStreamEvent),
    ) -> Result<ModelTurn, ModelError> {
        let result = (|| {
            if cancellation.is_cancelled() {
                return Err(ModelError::CancelledBeforeDispatch);
            }
            let body = self.request(task, tools, true)?;
            if cancellation.is_cancelled() {
                return Err(ModelError::CancelledBeforeDispatch);
            }
            let mut stream = stream::Stream::default();
            let (json_response, estimated_input_tokens) =
                self.http
                    .post_stream_observed(&body, cancellation, |event| {
                        stream.event(event, progress)
                    })?;
            let (response, usage) = match json_response.json {
                Some(response) => (response, None),
                None => stream.finish()?,
            };
            let mut turn = parse(response).map_err(ModelError::OutcomeUnknown)?;
            turn.usage = usage.or(turn.usage);
            turn.preflight =
                self.http
                    .preflight_report("anthropic", &self.model, estimated_input_tokens);
            Ok(turn)
        })();
        match &result {
            Ok(turn) if matches!(&turn.decision, ModelDecision::Call(_)) => {
                progress(ModelStreamEvent::Reset);
            }
            Err(_) => progress(ModelStreamEvent::Reset),
            _ => {}
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn retains_thinking_signatures_and_rejects_truncation() {
        let content = json!([{"type":"thinking","thinking":"reason","signature":"sig"},
            {"type":"tool_use","id":"tool_123","name":"artifact_create","input":{"title":"x","content":"y"}}]);
        let turn = parse(json!({"stop_reason":"tool_use","content":content})).unwrap();
        assert_eq!(turn.replay.unwrap().data, content);
        assert!(
            matches!(turn.decision, ModelDecision::Call(ToolCall { id, .. }) if id == "tool_123")
        );
        assert!(parse(json!({"stop_reason":"max_tokens","content":[]})).is_err());
        assert!(parse(json!({"stop_reason":"tool_use","content":[]})).is_err());
    }
}
