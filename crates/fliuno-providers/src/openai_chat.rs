//! OpenAI Chat Completions wire format. A model step can propose one tool.
use crate::{
    BudgetConfig,
    http::{Http, provider_headers},
};
use fliuno_config::{Provider, ReasoningEffort};
use fliuno_core::{
    CancellationToken, ModelDecision, ModelError, ModelProvider, ModelStreamEvent, ModelTurn,
    ProviderReplay, ToolSpec,
};
use fliuno_protocol::{
    MAX_TOOL_INPUT_BYTES, Message, ModelBudget, ModelInputType, ModelUsage, Task, ToolCall,
    UserContentPart,
};
use serde_json::{Value, json};

const PROVIDER: &str = "openai-chat";
pub(crate) struct ChatProvider {
    http: Http,
    model: String,
    effort: Option<ReasoningEffort>,
}

impl ChatProvider {
    pub(crate) fn new(
        base: &str,
        key: Option<&str>,
        model: &str,
        effort: Option<ReasoningEffort>,
        budget: Option<BudgetConfig>,
    ) -> Result<Self, String> {
        if model.trim().is_empty() {
            return Err("The model name must not be empty".into());
        }
        let budget = budget.map(|config| {
            if config.provider_count {
                return Err("Chat Completions does not provide an input count endpoint; set provider_count = false".into());
            }
            config.resolve(|_| Err("Chat Completions needs a numeric context window".into()))
        }).transpose()?;
        let mut http = Http::new_with_auth(
            base,
            "chat/completions",
            "chat/completions",
            &[],
            key.map(|key| provider_headers(Provider::OpenAi, key))
                .transpose()?,
        )?;
        if let Some(budget) = budget {
            http = http.with_context_budget(budget);
        }
        Ok(Self {
            http,
            model: model.into(),
            effort,
        })
    }

    fn request(&self, task: &Task, tools: &[ToolSpec], streaming: bool) -> Result<Value, String> {
        let mut messages =
            vec![json!({"role":"system","content":fliuno_core::agent_instructions()})];
        if let Some(history) =
            fliuno_core::task_context_prompt(task).map_err(|error| error.to_string())?
        {
            messages.push(json!({"role":"user","content":history}));
        }
        for message in &task.messages {
            match message {
                Message::User { text } => messages.push(json!({"role":"user","content":text})),
                Message::UserParts { parts } => {
                    messages.push(json!({"role":"user","content":content_parts(parts)?}))
                }
                Message::ProviderReplay { provider, data }
                    if provider == PROVIDER && data["role"] == "assistant" =>
                {
                    messages.push(data.clone());
                }
                Message::ProviderReplay { .. } => {
                    return Err("cannot switch model protocols inside a task".into());
                }
                Message::ToolResult { call_id, output } => messages.push(
                    json!({"role":"tool","tool_call_id":call_id,"content":output.to_string()}),
                ),
                Message::ToolCall { .. } | Message::Assistant { .. } => {}
            }
        }
        let mut body = json!({"model":self.model,"messages":messages});
        if !tools.is_empty() {
            body["tools"] = json!(tools.iter().map(|tool| json!({
                "type":"function", "function":{"name":tool.name,"description":tool.description,"parameters":tool.input_schema}
            })).collect::<Vec<_>>());
            body["parallel_tool_calls"] = json!(false);
        }
        if let Some(effort) = self.effort {
            body["reasoning_effort"] = json!(effort.as_str());
        }
        if let Some(limit) = self.http.max_output_tokens() {
            body["max_completion_tokens"] = json!(limit);
        }
        if streaming {
            body["stream"] = json!(true);
            body["stream_options"] = json!({"include_usage":true});
        }
        Ok(body)
    }

    fn run(
        &self,
        task: &Task,
        tools: &[ToolSpec],
        cancellation: &CancellationToken,
        streaming: bool,
        progress: &mut dyn FnMut(ModelStreamEvent),
    ) -> Result<ModelTurn, ModelError> {
        if cancellation.is_cancelled() {
            return Err(ModelError::CancelledBeforeDispatch);
        }
        let body = self
            .request(task, tools, streaming)
            .map_err(ModelError::NotDispatched)?;
        if !streaming {
            let (response, _) = self.http.post_observed(&body, cancellation)?;
            return reply(response).map_err(ModelError::OutcomeUnknown);
        }
        let mut stream = ChatStream::default();
        let (response, _) = self
            .http
            .post_stream_observed(&body, cancellation, |event| stream.event(event, progress))?;
        match response.json {
            Some(value) => reply(value),
            None => stream.finish(response.done),
        }
        .map_err(ModelError::OutcomeUnknown)
    }
}

fn content_parts(parts: &[UserContentPart]) -> Result<Vec<Value>, String> {
    parts.iter().map(|part| match part {
        UserContentPart::Text { text } => Ok(json!({"type":"text","text":text})),
        UserContentPart::Image { mime_type, data } => Ok(json!({"type":"image_url","image_url":{"url":format!("data:{};base64,{data}",mime_type.as_str())}})),
        UserContentPart::Pdf { .. } => Err("PDF input is unavailable for Chat Completions".into()),
    }).collect()
}

impl ModelProvider for ChatProvider {
    fn configured_budget(&self) -> Option<ModelBudget> {
        self.http.configured_budget()
    }

    fn model_identity(&self) -> Option<(String, String)> {
        Some((PROVIDER.into(), self.model.clone()))
    }

    fn reasoning_effort(&self) -> Option<&'static str> {
        self.effort.map(ReasoningEffort::as_str)
    }

    fn supported_input_types(&self) -> Vec<ModelInputType> {
        vec![ModelInputType::Text, ModelInputType::Image]
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
        self.run(task, tools, cancellation, false, &mut |_| {})
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
        let result = self.run(task, tools, cancellation, true, progress);
        if !matches!(&result, Ok(turn) if matches!(turn.decision, ModelDecision::Complete(_))) {
            progress(ModelStreamEvent::Reset);
        }
        result
    }
}

fn reply(response: Value) -> Result<ModelTurn, String> {
    let choices = choices(&response)?;
    let [choice] = choices else {
        return Err("Chat Completions requires one response choice".into());
    };
    let message = &choice["message"];
    if !first_choice(choice) || message["role"] != "assistant" {
        return Err("Chat Completions returned an invalid assistant role".into());
    }
    turn(
        choice["finish_reason"].as_str(),
        message["content"].as_str().unwrap_or_default().into(),
        message["refusal"].as_str().unwrap_or_default().into(),
        single_call(message)?,
        usage(&response["usage"]),
    )
}

fn choices(value: &Value) -> Result<&[Value], String> {
    value["choices"]
        .as_array()
        .map(Vec::as_slice)
        .ok_or_else(|| "Chat Completions returned invalid choices".into())
}

fn first_choice(choice: &Value) -> bool {
    choice
        .get("index")
        .is_none_or(|index| index.as_u64() == Some(0))
}

fn turn(
    reason: Option<&str>,
    content: String,
    refusal: String,
    call: Option<Function>,
    usage: Option<ModelUsage>,
) -> Result<ModelTurn, String> {
    let mut replay = json!({"role":"assistant","content":content});
    if !refusal.is_empty() {
        replay["refusal"] = json!(refusal);
    }
    let decision = match (reason, call) {
        (Some("stop"), None) if !content.is_empty() || !refusal.is_empty() => {
            ModelDecision::Complete(content + &refusal)
        }
        (Some("tool_calls"), Some(call)) => {
            let arguments: Value = serde_json::from_str(&call.arguments)
                .map_err(|_| "Chat Completions returned invalid function arguments")?;
            if call.id.is_empty() || call.name.is_empty() || !arguments.is_object() {
                return Err("Chat Completions returned an incomplete function call".into());
            }
            replay["tool_calls"] = json!([{"id":call.id,"type":"function","function":{"name":call.name,"arguments":call.arguments}}]);
            ModelDecision::Call(ToolCall {
                id: call.id,
                name: call.name,
                arguments,
            })
        }
        _ => {
            return Err(
                "Chat Completions did not finish with text or one complete function call".into(),
            );
        }
    };
    Ok(ModelTurn {
        decision,
        replay: Some(ProviderReplay {
            provider: PROVIDER.into(),
            data: replay,
        }),
        usage,
        preflight: None,
    })
}

fn usage(value: &Value) -> Option<ModelUsage> {
    let input_tokens = value["prompt_tokens"].as_u64()?;
    let output_tokens = value["completion_tokens"].as_u64()?;
    Some(ModelUsage {
        provider: PROVIDER.into(),
        input_tokens,
        output_tokens,
        cached_input_tokens: value["prompt_tokens_details"]["cached_tokens"]
            .as_u64()
            .filter(|count| *count <= input_tokens),
        reasoning_output_tokens: value["completion_tokens_details"]["reasoning_tokens"]
            .as_u64()
            .filter(|count| *count <= output_tokens),
    })
}

fn single_call(message: &Value) -> Result<Option<Function>, String> {
    match message.get("tool_calls") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Array(calls)) => match calls.as_slice() {
            [] => Ok(None),
            [call] => {
                if call["type"] != "function" {
                    return Err("Chat Completions requires function tool calls".into());
                }
                let mut function = Function::default();
                function.append(call)?;
                Ok(Some(function))
            }
            _ => Err("Chat Completions returned parallel calls in serial mode".into()),
        },
        _ => Err("Chat Completions returned invalid tool calls".into()),
    }
}

#[derive(Default)]
struct Function {
    id: String,
    name: String,
    arguments: String,
}

impl Function {
    fn append(&mut self, fragment: &Value) -> Result<(), String> {
        append(&mut self.id, &fragment["id"], 256)?;
        append(&mut self.name, &fragment["function"]["name"], 256)?;
        append(
            &mut self.arguments,
            &fragment["function"]["arguments"],
            MAX_TOOL_INPUT_BYTES,
        )
    }
}

fn append(output: &mut String, fragment: &Value, limit: usize) -> Result<(), String> {
    if fragment.is_null() {
        return Ok(());
    }
    let fragment = fragment
        .as_str()
        .ok_or("Chat Completions returned an invalid text field")?;
    if fragment.len() > limit.saturating_sub(output.len()) {
        return Err("Chat Completions field exceeds its size limit".into());
    }
    output.push_str(fragment);
    Ok(())
}

#[derive(Default)]
struct ChatStream {
    content: String,
    refusal: String,
    call: Option<Function>,
    reason: Option<String>,
    usage: Option<ModelUsage>,
}

impl ChatStream {
    fn event(
        &mut self,
        event: Value,
        progress: &mut dyn FnMut(ModelStreamEvent),
    ) -> Result<(), String> {
        let choices = choices(&event)?;
        if let Some(usage) = usage(&event["usage"]) {
            self.usage = Some(usage);
        }
        let choice = match choices {
            [] => return Ok(()),
            [choice] if first_choice(choice) && self.reason.is_none() => choice,
            _ => return Err("Chat Completions returned invalid stream choices".into()),
        };
        let delta = &choice["delta"];
        if !delta["role"].is_null() && delta["role"] != "assistant" {
            return Err("Chat Completions returned an invalid stream role".into());
        }
        for (text, fragment) in [
            (&mut self.content, &delta["content"]),
            (&mut self.refusal, &delta["refusal"]),
        ] {
            append(text, fragment, 4 * 1024 * 1024)?;
            if let Some(fragment) = fragment.as_str().filter(|fragment| !fragment.is_empty()) {
                progress(ModelStreamEvent::TextDelta(fragment.into()));
            }
        }
        if !delta["tool_calls"].is_null() {
            let calls = delta["tool_calls"]
                .as_array()
                .ok_or("Chat Completions returned invalid tool deltas")?;
            if calls.len() > 1 {
                return Err("Chat Completions returned parallel tool calls".into());
            }
            for call in calls {
                if call["index"].as_u64() != Some(0)
                    || (!call["type"].is_null() && call["type"] != "function")
                {
                    return Err(
                        "Chat Completions returned unsupported or parallel tool calls".into(),
                    );
                }
                self.call
                    .get_or_insert_with(Function::default)
                    .append(call)?;
            }
        }
        self.reason = choice["finish_reason"].as_str().map(str::to_owned);
        Ok(())
    }

    fn finish(self, done: bool) -> Result<ModelTurn, String> {
        if !done {
            return Err("Chat Completions stream ended without its completion marker".into());
        }
        turn(
            self.reason.as_deref(),
            self.content,
            self.refusal,
            self.call,
            self.usage,
        )
    }
}
