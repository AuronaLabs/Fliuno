use super::*;
use fliuno_config::Settings;
use fliuno_core::{ToolEffect, ToolSpec};

fn provider_at(base: &str, options: &str) -> Box<dyn ModelProvider> {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("key"), "fixture-key\n").unwrap();
    let config = dir.path().join("config.toml");
    std::fs::write(
        &config,
        format!(
            r#"
[model]
provider = "openai"
protocol = "chat_completions"
name = "fixture-model"
base_url = "{base}"
api_key_file = "key"
{options}
"#
        ),
    )
    .unwrap();
    let settings = Settings::load(&config).unwrap();
    let (name, provider) = fliuno_providers::from_settings(&settings.model).unwrap();
    assert_eq!(name, "openai-chat");
    provider
}

fn chunk(delta: Value, reason: Option<&str>) -> Value {
    json!({"choices":[{"index":0,"delta":delta,"finish_reason":reason}]})
}

fn plain_reply() -> Value {
    json!({
        "choices":[{"index":0,"message":{"role":"assistant","content":"The report is ready."},"finish_reason":"stop"}],
        "usage":{"prompt_tokens":96,"completion_tokens":8,"prompt_tokens_details":{"cached_tokens":64},"completion_tokens_details":{"reasoning_tokens":5}}
    })
}

#[test]
fn chat_sends_native_request_and_reads_json_usage() {
    let (base, wire) = fixture(vec![(200, plain_reply())]);
    let provider = provider_at(
        &base,
        "reasoning_effort = 'high'\n[model.budget]\nwindow = 8192\nmax_output_tokens = 128",
    );
    let (task, _) = prepared_model_step();
    let result = provider
        .next_classified(&task, &[], &CancellationToken::new())
        .unwrap();
    assert!(
        matches!(result.decision, ModelDecision::Complete(text) if text == "The report is ready.")
    );
    let usage = result.usage.unwrap();
    assert_eq!((usage.input_tokens, usage.output_tokens), (96, 8));
    assert_eq!(usage.cached_input_tokens, Some(64));
    assert_eq!(usage.reasoning_output_tokens, Some(5));
    assert_eq!(
        provider.persistent_identity().unwrap().0,
        "openai-chat@effort=high"
    );
    let requests = wire.join().unwrap();
    let (headers, body) = &requests[0];
    assert!(headers.starts_with("POST /v1/chat/completions "));
    assert!(
        headers
            .to_ascii_lowercase()
            .contains("authorization: bearer fixture-key")
    );
    assert_eq!(body["reasoning_effort"], "high");
    assert_eq!(body["max_completion_tokens"], 128);
    assert_eq!(body["messages"][0]["role"], "system");
    assert_eq!(
        body["messages"][1],
        json!({"role":"user","content":"stream fixture"})
    );
    assert!(body.get("tools").is_none());
}

#[test]
fn chat_assembles_streamed_function_and_replays_approved_result() {
    let events = [
        chunk(
            json!({"role":"assistant","content":"I'll prepare it."}),
            None,
        ),
        chunk(
            json!({"tool_calls":[{"index":0,"id":"chat-call","type":"function","function":{"name":"fixture_tool","arguments":"{\"ti"}}]}),
            None,
        ),
        chunk(
            json!({"tool_calls":[{"index":0,"function":{"arguments":"tle\":\"Report\"}"}}]}),
            None,
        ),
        chunk(json!({}), Some("tool_calls")),
        json!({"choices":[],"usage":{"prompt_tokens":16,"completion_tokens":7}}),
    ];
    let (base, wire) = sse_fixture(sse_body(&events) + "data: [DONE]\r\n\r\n");
    let provider = provider_at(&base, "");
    let (mut task, _) = prepared_model_step();
    let tools = [ToolSpec {
        name: "fixture_tool".into(),
        description: "Fixture operation".into(),
        effect: ToolEffect::ArtifactWrite,
        input_schema: json!({"type":"object"}),
    }];
    let mut progress = Vec::new();
    let result = provider
        .next_with_progress_classified(&task, &tools, &CancellationToken::new(), &mut |event| {
            progress.push(event)
        })
        .unwrap();
    let ModelDecision::Call(call) = result.decision else {
        panic!("expected complete tool proposal")
    };
    assert_eq!(call.id, "chat-call");
    assert_eq!(call.name, "fixture_tool");
    assert_eq!(call.arguments, json!({"title":"Report"}));
    assert_eq!(result.usage.unwrap().output_tokens, 7);
    assert_eq!(
        progress,
        [
            ModelStreamEvent::TextDelta("I'll prepare it.".into()),
            ModelStreamEvent::Reset
        ]
    );
    let (_, body) = wire.join().unwrap();
    assert_eq!(body["stream"], true);
    assert_eq!(body["stream_options"]["include_usage"], true);
    assert_eq!(body["parallel_tool_calls"], false);
    assert_eq!(body["tools"][0]["function"]["name"], "fixture_tool");
    assert!(body.get("reasoning_effort").is_none());
    let replay = result.replay.unwrap();
    task.messages.push(Message::ProviderReplay {
        provider: replay.provider,
        data: replay.data,
    });
    task.messages.push(Message::ToolCall { call });
    task.messages.push(Message::ToolResult {
        call_id: "chat-call".into(),
        output: json!({"verified":true}),
    });
    let (base, wire) = fixture(vec![(200, plain_reply())]);
    // Gateways may send complete JSON for a streaming request.
    let result = provider_at(&base, "")
        .next_with_progress_classified(&task, &tools, &CancellationToken::new(), &mut |_| {})
        .unwrap();
    assert!(matches!(result.decision, ModelDecision::Complete(_)));
    let messages = &wire.join().unwrap()[0].1["messages"];
    assert_eq!(messages.as_array().unwrap().len(), 4);
    assert_eq!(messages[2]["tool_calls"][0]["id"], "chat-call");
    assert_eq!(
        messages[2]["tool_calls"][0]["function"]["arguments"],
        "{\"title\":\"Report\"}"
    );
    assert_eq!(
        messages[3],
        json!({"role":"tool","tool_call_id":"chat-call","content":"{\"verified\":true}"})
    );
}

#[test]
fn chat_discards_partial_and_parallel_streams_without_committing_tools() {
    let preview = chunk(json!({"content":"uncommitted"}), None);
    let cases = [
        sse_body(&[preview.clone(), chunk(json!({}), Some("stop"))]),
        sse_body(&[preview.clone()]) + "data: [DONE]\n\n",
        sse_body(&[preview.clone(), chunk(json!({}), Some("length"))]) + "data: [DONE]\n\n",
        sse_body(&[
            preview,
            chunk(
                json!({"tool_calls":[{"index":0,"id":"first"},{"index":1,"id":"second"}]}),
                None,
            ),
        ]) + "data: [DONE]\n\n",
    ];
    let (task, _) = prepared_model_step();
    for response in cases {
        let (base, wire) = sse_fixture(response);
        let provider = provider_at(&base, "");
        let mut updates = Vec::new();
        let result = provider.next_with_progress_classified(
            &task,
            &[],
            &CancellationToken::new(),
            &mut |event| updates.push(event),
        );
        assert!(matches!(result, Err(ModelError::OutcomeUnknown(_))));
        assert_eq!(
            updates,
            [
                ModelStreamEvent::TextDelta("uncommitted".into()),
                ModelStreamEvent::Reset
            ]
        );
        wire.join().unwrap();
    }
}

#[test]
fn chat_and_responses_reject_each_others_continuations_before_dispatch() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let base = format!("http://{}/v1/", listener.local_addr().unwrap());
    let chat = provider_at(&base, "");
    let responses = OpenAiProvider::new(&base, "fixture-key", "fixture-model").unwrap();
    assert_ne!(chat.persistent_identity(), responses.persistent_identity());
    let (mut task, _) = prepared_model_step();
    task.messages.push(Message::ProviderReplay {
        provider: "openai".into(),
        data: json!([]),
    });
    assert!(matches!(
        chat.next_classified(&task, &[], &CancellationToken::new()),
        Err(ModelError::NotDispatched(_))
    ));
    task.messages.pop();
    task.messages.push(Message::ProviderReplay {
        provider: "openai-chat".into(),
        data: json!({"role":"assistant","content":"earlier"}),
    });
    assert!(matches!(
        responses.next_classified(&task, &[], &CancellationToken::new()),
        Err(ModelError::NotDispatched(_))
    ));
    assert!(
        matches!(listener.accept(), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock)
    );
}
