//! Reconstruct complete provider blocks; partial tool JSON never becomes a call.
use super::usage::Usage;
use fliuno_core::ModelStreamEvent;
use fliuno_protocol::ModelUsage;
use serde_json::{Value, json};

#[derive(Default)]
pub(super) struct Stream {
    started: bool,
    stopped: bool,
    content: Vec<Value>,
    open_block: Option<usize>,
    partial_json: Option<String>,
    stop_reason: Option<String>,
    usage: Usage,
}

impl Stream {
    pub(super) fn event(
        &mut self,
        event: Value,
        progress: &mut dyn FnMut(ModelStreamEvent),
    ) -> Result<(), String> {
        let kind = event["type"]
            .as_str()
            .ok_or("Anthropic event has no type")?;
        if kind == "error" {
            // Remote error payloads may contain prompts or credentials.
            return Err("Anthropic stream reported an error".into());
        }
        if kind == "ping" {
            return Ok(());
        }
        if self.stopped {
            return Err("Anthropic event arrived after message_stop".into());
        }
        if kind == "message_start" {
            if self.started
                || !event["message"]["content"]
                    .as_array()
                    .is_some_and(Vec::is_empty)
            {
                return Err("invalid Anthropic message_start".into());
            }
            self.usage.observe_input(&event["message"]["usage"]);
            self.started = true;
            return Ok(());
        }
        // Unknown top-level events may be added by the provider. Known events
        // must still obey the message/block lifecycle.
        if !matches!(
            kind,
            "content_block_start"
                | "content_block_delta"
                | "content_block_stop"
                | "message_delta"
                | "message_stop"
        ) {
            return Ok(());
        }
        if !self.started {
            return Err("Anthropic content arrived before message_start".into());
        }
        match kind {
            "content_block_start" => {
                let index = block_index(&event)?;
                if self.open_block.is_some()
                    || self.stop_reason.is_some()
                    || index != self.content.len()
                {
                    return Err("Anthropic content block is duplicate or out of order".into());
                }
                let block = event
                    .get("content_block")
                    .cloned()
                    .ok_or("Anthropic event has no content block")?;
                if !matches!(
                    block["type"].as_str(),
                    Some("text" | "thinking" | "redacted_thinking" | "tool_use")
                ) {
                    return Err("unsupported Anthropic content block".into());
                }
                self.content.push(block);
                self.open_block = Some(index);
            }
            "content_block_delta" => {
                let index = block_index(&event)?;
                if self.open_block != Some(index) {
                    return Err("Anthropic delta does not match the open content block".into());
                }
                let block = &mut self.content[index];
                let delta = &event["delta"];
                match (block["type"].as_str(), delta["type"].as_str()) {
                    (Some("text"), Some("text_delta")) => {
                        let text = delta["text"]
                            .as_str()
                            .ok_or("Anthropic text delta is missing text")?;
                        append_field(block, "text", text)?;
                        if !text.is_empty() {
                            progress(ModelStreamEvent::TextDelta(text.into()));
                        }
                    }
                    (Some("thinking"), Some("thinking_delta")) => {
                        append_field(
                            block,
                            "thinking",
                            delta["thinking"]
                                .as_str()
                                .ok_or("Anthropic thinking delta is missing text")?,
                        )?;
                    }
                    (Some("thinking"), Some("signature_delta")) => {
                        append_field(
                            block,
                            "signature",
                            delta["signature"]
                                .as_str()
                                .ok_or("Anthropic signature delta is missing signature")?,
                        )?;
                    }
                    (Some("tool_use"), Some("input_json_delta")) => {
                        let partial = delta["partial_json"]
                            .as_str()
                            .ok_or("Anthropic input delta is missing JSON")?;
                        self.partial_json
                            .get_or_insert_with(String::new)
                            .push_str(partial);
                    }
                    _ => return Err("unsupported or mismatched Anthropic content delta".into()),
                }
            }
            "content_block_stop" => {
                let index = block_index(&event)?;
                if self.open_block != Some(index) {
                    return Err("Anthropic stopped an unknown or closed content block".into());
                }
                if let Some(partial) = self.partial_json.take() {
                    let input: Value = serde_json::from_str(&partial)
                        .map_err(|_| "Anthropic returned invalid tool input JSON")?;
                    if !input.is_object() {
                        return Err("Anthropic tool input must be an object".into());
                    }
                    self.content[index]["input"] = input;
                }
                self.open_block = None;
            }
            "message_delta" => {
                if self.open_block.is_some() {
                    return Err("Anthropic message_delta arrived before content_block_stop".into());
                }
                self.usage.observe_complete(&event["usage"]);
                if let Some(reason) = event["delta"]["stop_reason"].as_str() {
                    self.stop_reason = Some(reason.into());
                }
            }
            "message_stop" => {
                if self.open_block.is_some() || self.stop_reason.is_none() {
                    return Err("Anthropic message_stop arrived before all blocks completed".into());
                }
                self.stopped = true;
            }
            _ => unreachable!("known events were checked above"),
        }
        Ok(())
    }

    pub(super) fn finish(self) -> Result<(Value, Option<ModelUsage>), String> {
        if !self.stopped {
            return Err("Anthropic event stream ended before message_stop".into());
        }
        Ok((
            json!({"stop_reason":self.stop_reason,"content":self.content}),
            self.usage.report(),
        ))
    }
}

fn append_field(value: &mut Value, field: &str, suffix: &str) -> Result<(), String> {
    let object = value
        .as_object_mut()
        .ok_or("invalid Anthropic content block")?;
    let text = object.entry(field.to_owned()).or_insert_with(|| json!(""));
    if let Value::String(text) = text {
        text.push_str(suffix);
        Ok(())
    } else {
        Err("invalid Anthropic content block".into())
    }
}

fn block_index(event: &Value) -> Result<usize, String> {
    event["index"]
        .as_u64()
        .filter(|index| *index < 1024)
        .map(|index| index as usize)
        .ok_or_else(|| "Anthropic content block index is invalid or exceeds limit".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unfinished_tool_block_never_yields_a_complete_turn() {
        let mut stream = Stream::default();
        let mut updates = Vec::new();
        for event in [
            json!({"type":"message_start","message":{"content":[]}}),
            json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"preview"}}),
            json!({"type":"content_block_stop","index":0}),
            json!({"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"call_1","name":"artifact_create","input":{}}}),
            json!({"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"title\":\"x\"}"}}),
        ] {
            stream
                .event(event, &mut |update| updates.push(update))
                .unwrap();
        }
        assert!(stream.finish().is_err());
        assert_eq!(updates, vec![ModelStreamEvent::TextDelta("preview".into())]);
    }
}
