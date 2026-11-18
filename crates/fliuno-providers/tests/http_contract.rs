//! Loopback fixtures test wire requests and a complete kernel/tool round trip.
//! They never use environment credentials or contact a real model service.
use fliuno_core::{
    CancellationToken, DemoProvider, Kernel, ModelDecision, ModelError, ModelProvider,
    ModelStreamEvent,
};
use fliuno_protocol::{ApprovalDecision, Message, Session, Task, TaskStatus};
use fliuno_providers::{AnthropicProvider, OpenAiProvider};
use serde_json::{Value, json};
use std::{
    io::{BufRead, BufReader, Read, Write},
    net::{TcpListener, TcpStream},
    thread,
    time::Duration,
};

fn read_request(stream: &mut TcpStream) -> (String, Value) {
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let mut reader = BufReader::new(stream);
    let mut headers = String::new();
    let mut length = 0;
    loop {
        let mut line = String::new();
        assert!(reader.read_line(&mut line).unwrap() > 0);
        if line == "\r\n" {
            break;
        }
        if let Some((key, value)) = line.split_once(':')
            && key.eq_ignore_ascii_case("content-length")
        {
            length = value.trim().parse::<usize>().unwrap();
        }
        headers.push_str(&line);
    }
    assert!(length < 1024 * 1024);
    let mut body = vec![0; length];
    reader.read_exact(&mut body).unwrap();
    (headers, serde_json::from_slice(&body).unwrap())
}

fn fixture(responses: Vec<(u16, Value)>) -> (String, thread::JoinHandle<Vec<(String, Value)>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}/v1/", listener.local_addr().unwrap());
    let handle = thread::spawn(move || {
        let mut requests = Vec::new();
        for (status, body) in responses {
            let (mut stream, _) = listener.accept().unwrap();
            requests.push(read_request(&mut stream));
            let body = body.to_string();
            write!(stream,"HTTP/1.1 {status} Fixture\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).unwrap();
        }
        requests
    });
    (base, handle)
}

fn sse_fixture(body: String) -> (String, thread::JoinHandle<(String, Value)>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}/v1/", listener.local_addr().unwrap());
    let handle = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let request = read_request(&mut stream);
        write!(stream,"HTTP/1.1 200 Fixture\r\nContent-Type: text/event-stream; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).unwrap();
        request
    });
    (base, handle)
}

fn sse_body(events: &[Value]) -> String {
    events
        .iter()
        .map(|event| format!("event: {}\ndata: {}\n\n", event["type"], event))
        .collect()
}

fn prepared_model_step() -> (Task, Vec<fliuno_core::ToolSpec>) {
    let mut kernel = Kernel::in_memory(Box::new(DemoProvider)).unwrap();
    let workspace = kernel
        .open_workspace(std::env::current_dir().unwrap().to_str().unwrap())
        .unwrap();
    let session = kernel.create_session(&workspace.id).unwrap();
    let (task, _) = kernel
        .create_task_with_key(&session.id, "stream fixture", None)
        .unwrap();
    let fliuno_core::PreparedModelStep::Ready { task, tools } =
        kernel.prepare_model_step(&task.id).unwrap()
    else {
        panic!("fixture task did not prepare a model step");
    };
    (task, tools)
}

#[test]
fn openai_streams_text_and_commits_only_the_completed_response() {
    let events = vec![
        json!({"type":"response.output_text.delta","delta":"Hel"}),
        json!({"type":"response.output_text.delta","delta":"lo"}),
        json!({"type":"response.completed","response":{"status":"completed","output":[{"type":"message","content":[{"type":"output_text","text":"Hello"}]}]}}),
    ];
    let (base, handle) = sse_fixture(sse_body(&events));
    let provider = OpenAiProvider::new(&base, "fixture-key", "fixture-model").unwrap();
    let (task, tools) = prepared_model_step();
    let mut updates = Vec::new();
    let turn = provider
        .next_with_progress(&task, &tools, &CancellationToken::new(), &mut |event| {
            updates.push(event)
        })
        .unwrap();
    assert!(matches!(turn.decision, ModelDecision::Complete(text) if text == "Hello"));
    assert_eq!(
        updates,
        vec![
            ModelStreamEvent::TextDelta("Hel".into()),
            ModelStreamEvent::TextDelta("lo".into()),
        ]
    );
    let (headers, body) = handle.join().unwrap();
    assert!(
        headers
            .to_ascii_lowercase()
            .contains("accept: text/event-stream")
    );
    assert_eq!(body["stream"], true);
}

#[test]
fn pre_dispatch_rejection_and_cancellation_never_open_a_provider_connection() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let base = format!("http://{}/v1/", listener.local_addr().unwrap());
    let provider = OpenAiProvider::new(&base, "fixture-key", "fixture-model").unwrap();
    let (mut task, tools) = prepared_model_step();
    task.messages.push(Message::ProviderReplay {
        provider: "anthropic".into(),
        data: json!([]),
    });
    let rejected = provider
        .next_classified(&task, &tools, &CancellationToken::new())
        .err()
        .unwrap();
    assert!(matches!(rejected, ModelError::NotDispatched(_)));
    assert!(
        matches!(listener.accept(), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock)
    );

    task.messages.pop();
    let cancellation = CancellationToken::new();
    cancellation.cancel();
    let cancelled = provider
        .next_classified(&task, &tools, &cancellation)
        .err()
        .unwrap();
    assert_eq!(cancelled, ModelError::CancelledBeforeDispatch);
    assert!(
        matches!(listener.accept(), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock)
    );
}

#[test]
fn anthropic_rebuilds_signed_tool_blocks_and_clears_text_preview() {
    let events = vec![
        json!({"type":"message_start","message":{"id":"m","content":[]}}),
        json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"reason"}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"sig"}}),
        json!({"type":"content_block_stop","index":0}),
        json!({"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"call_1","name":"artifact_create","input":{}}}),
        json!({"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"title\":\"x\",\"content\":\"y\"}"}}),
        json!({"type":"content_block_stop","index":1}),
        json!({"type":"message_delta","delta":{"stop_reason":"tool_use"}}),
        json!({"type":"message_stop"}),
    ];
    let (base, handle) = sse_fixture(sse_body(&events));
    let provider = AnthropicProvider::new(&base, "fixture-key", "fixture-model").unwrap();
    let (task, tools) = prepared_model_step();
    let mut updates = Vec::new();
    let turn = provider
        .next_with_progress(&task, &tools, &CancellationToken::new(), &mut |event| {
            updates.push(event)
        })
        .unwrap();
    assert!(
        matches!(turn.decision, ModelDecision::Call(call) if call.id == "call_1" && call.arguments == json!({"title":"x","content":"y"}))
    );
    assert_eq!(
        turn.replay.unwrap().data,
        json!([
            {"type":"thinking","thinking":"reason","signature":"sig"},
            {"type":"tool_use","id":"call_1","name":"artifact_create","input":{"title":"x","content":"y"}}
        ])
    );
    assert_eq!(updates, vec![ModelStreamEvent::Reset]);
    let (_, body) = handle.join().unwrap();
    assert_eq!(body["stream"], true);
}

#[test]
fn anthropic_json_compatible_endpoint_still_uses_the_complete_response() {
    let (base, handle) = fixture(vec![(
        200,
        json!({"stop_reason":"end_turn","content":[{"type":"text","text":"Done"}]}),
    )]);
    let provider = AnthropicProvider::new(&base, "fixture-key", "fixture-model").unwrap();
    let (task, tools) = prepared_model_step();
    let mut updates = Vec::new();
    let turn = provider
        .next_with_progress(&task, &tools, &CancellationToken::new(), &mut |event| {
            updates.push(event)
        })
        .unwrap();
    assert!(matches!(turn.decision, ModelDecision::Complete(text) if text == "Done"));
    assert!(updates.is_empty());
    let requests = handle.join().unwrap();
    assert_eq!(requests[0].1["stream"], true);
}

#[test]
fn incomplete_openai_stream_resets_preview_and_keeps_outcome_unknown() {
    let events = vec![json!({"type":"response.output_text.delta","delta":"partial"})];
    let (base, handle) = sse_fixture(sse_body(&events));
    let provider = OpenAiProvider::new(&base, "fixture-key", "fixture-model").unwrap();
    let (task, tools) = prepared_model_step();
    let mut updates = Vec::new();
    assert!(matches!(
        provider.next_with_progress_classified(
            &task,
            &tools,
            &CancellationToken::new(),
            &mut |event| updates.push(event),
        ),
        Err(ModelError::OutcomeUnknown(_))
    ));
    assert_eq!(
        updates,
        vec![
            ModelStreamEvent::TextDelta("partial".into()),
            ModelStreamEvent::Reset,
        ]
    );
    handle.join().unwrap();
    assert!(task.model_outcome_unknown);
}

fn roundtrip(provider: Box<dyn ModelProvider>) -> (Kernel, Session, Task) {
    let mut kernel = Kernel::in_memory(provider).unwrap();
    let ws = kernel
        .open_workspace(std::env::current_dir().unwrap().to_str().unwrap())
        .unwrap();
    let session = kernel.create_session(&ws.id).unwrap();
    let task = kernel.start_task(&session.id, "Create a report").unwrap();
    assert_eq!(task.status, TaskStatus::WaitingApproval);
    assert!(kernel.list_artifacts(&task.id).unwrap().is_empty());
    let done = kernel
        .respond_approval(
            &task.id,
            &task.pending_approval.unwrap().id,
            ApprovalDecision::AllowOnce,
        )
        .unwrap();
    assert_eq!(done.status, TaskStatus::Completed);
    assert_eq!(
        kernel.list_artifacts(&task.id).unwrap()[0].content,
        "report body"
    );
    (kernel, session, done)
}

/// Assert against the actual HTTP request, rather than the persisted snapshot
/// alone: a new task must send history as data without replaying old tool calls.
fn assert_history(body: &Value, field: &str, kernel: &Kernel, source: &Task) {
    let messages = body[field].as_array().unwrap();
    assert_eq!(messages.len(), 2);
    assert!(messages.iter().all(|message| message["role"] == "user"));
    assert_eq!(messages[1]["content"], "Follow up");
    let text = messages[0]["content"].as_str().unwrap();
    let (_, data) = text.split_once('\n').unwrap();
    let context: Value = serde_json::from_str(data).unwrap();
    assert_eq!(context["turns"].as_array().unwrap().len(), 1);
    let turn = &context["turns"][0];
    assert_eq!(turn["taskId"], source.id);
    assert_eq!(turn["messages"][0]["text"], "Create a report");
    assert_eq!(turn["messages"][1]["role"], "tool_call");
    assert_eq!(turn["messages"][2]["role"], "tool_result");
    assert_eq!(turn["messages"][3]["text"], "Done");
    assert_eq!(
        turn["artifacts"][0]["id"],
        kernel.list_artifacts(&source.id).unwrap()[0].id
    );
    for private in [
        "encrypted",
        "signature",
        "provider_replay",
        "pendingApproval",
    ] {
        assert!(!text.contains(private), "history leaked {private}");
    }
}

fn openai_done() -> Value {
    json!({"status":"completed","output":[{"type":"message","content":[{"type":"output_text","text":"Done"}]}]})
}

fn anthropic_done() -> Value {
    json!({"stop_reason":"end_turn","content":[{"type":"text","text":"Done"}]})
}

#[test]
fn openai_function_result_and_reasoning_are_replayed() {
    let output = json!([{"type":"reasoning","id":"r","encrypted_content":"encrypted"},{"type":"function_call","call_id":"c","name":"artifact_create","arguments":"{\"title\":\"Report\",\"content\":\"report body\"}"}]);
    let (base, handle) = fixture(vec![
        (200, json!({"status":"completed","output":output})),
        (
            200,
            json!({"status":"completed","output":[{"type":"message","content":[{"type":"output_text","text":"Done"}]}]}),
        ),
        (200, openai_done()),
    ]);
    let (mut kernel, session, source) = roundtrip(Box::new(
        OpenAiProvider::new(&base, "fixture-key", "fixture-model").unwrap(),
    ));
    assert_eq!(
        kernel.start_task(&session.id, "Follow up").unwrap().status,
        TaskStatus::Completed
    );
    let requests = handle.join().unwrap();
    assert!(requests[0].0.starts_with("POST /v1/responses "));
    assert_eq!(requests[0].1["parallel_tool_calls"], false);
    assert_eq!(requests[0].1["store"], false);
    assert_eq!(requests[1].1["input"][1], output[0]);
    assert_eq!(requests[1].1["input"][2], output[1]);
    assert_eq!(requests[1].1["input"][3]["type"], "function_call_output");
    assert_eq!(requests[1].1["input"][3]["call_id"], "c");
    assert!(
        requests[0]
            .0
            .to_lowercase()
            .contains("authorization: bearer fixture-key")
    );
    assert_history(&requests[2].1, "input", &kernel, &source);
}

#[test]
fn anthropic_tool_result_and_thinking_are_replayed() {
    let content = json!([{"type":"thinking","thinking":"reason","signature":"sig"},{"type":"tool_use","id":"c","name":"artifact_create","input":{"title":"Report","content":"report body"}}]);
    let (base, handle) = fixture(vec![
        (200, json!({"stop_reason":"tool_use","content":content})),
        (
            200,
            json!({"stop_reason":"end_turn","content":[{"type":"text","text":"Done"}]}),
        ),
        (200, anthropic_done()),
    ]);
    let (mut kernel, session, source) = roundtrip(Box::new(
        AnthropicProvider::new(&base, "fixture-key", "fixture-model").unwrap(),
    ));
    assert_eq!(
        kernel.start_task(&session.id, "Follow up").unwrap().status,
        TaskStatus::Completed
    );
    let requests = handle.join().unwrap();
    assert!(requests[0].0.starts_with("POST /v1/messages "));
    assert_eq!(requests[1].1["messages"][1]["content"], content);
    assert_eq!(
        requests[1].1["messages"][2]["content"][0]["tool_use_id"],
        "c"
    );
    assert!(
        requests[0]
            .0
            .to_lowercase()
            .contains("anthropic-version: 2023-06-01")
    );
    assert_history(&requests[2].1, "messages", &kernel, &source);
}

#[test]
fn new_tasks_can_change_provider_without_replaying_signed_continuations() {
    for from_openai in [true, false] {
        let first_turn = if from_openai {
            json!({"status":"completed","output":[
                {"type":"reasoning","id":"r","encrypted_content":"encrypted"},
                {"type":"function_call","call_id":"c","name":"artifact_create","arguments":"{\"title\":\"Report\",\"content\":\"report body\"}"}
            ]})
        } else {
            json!({"stop_reason":"tool_use","content":[
                {"type":"thinking","thinking":"reason","signature":"sig"},
                {"type":"tool_use","id":"c","name":"artifact_create","input":{"title":"Report","content":"report body"}}
            ]})
        };
        let (base, source_wire) = fixture(vec![
            (200, first_turn),
            (
                200,
                if from_openai {
                    openai_done()
                } else {
                    anthropic_done()
                },
            ),
        ]);
        let source_provider: Box<dyn ModelProvider> = if from_openai {
            Box::new(OpenAiProvider::new(&base, "fixture-key", "model").unwrap())
        } else {
            Box::new(AnthropicProvider::new(&base, "fixture-key", "model").unwrap())
        };
        let (mut kernel, session, source) = roundtrip(source_provider);
        source_wire.join().unwrap();
        let (base, target_wire) = fixture(vec![(
            200,
            if from_openai {
                anthropic_done()
            } else {
                openai_done()
            },
        )]);
        let target: Box<dyn ModelProvider> = if from_openai {
            Box::new(AnthropicProvider::new(&base, "fixture-key", "model").unwrap())
        } else {
            Box::new(OpenAiProvider::new(&base, "fixture-key", "model").unwrap())
        };
        // The same task still requires its original provider's opaque blocks.
        assert!(
            target
                .next(&source, &[], &CancellationToken::new())
                .err()
                .unwrap()
                .contains("cannot switch providers")
        );
        kernel.set_model_provider(target);
        assert_eq!(
            kernel.start_task(&session.id, "Follow up").unwrap().status,
            TaskStatus::Completed
        );
        let wire = target_wire.join().unwrap();
        assert_history(
            &wire[0].1,
            if from_openai { "messages" } else { "input" },
            &kernel,
            &source,
        );
    }
}

#[test]
fn remote_error_bodies_never_enter_task_history() {
    let (base, handle) = fixture(vec![(
        401,
        json!({"error":"echoed-sensitive-fixture-value"}),
    )]);
    let mut kernel = Kernel::in_memory(Box::new(
        OpenAiProvider::new(&base, "fixture-key", "model").unwrap(),
    ))
    .unwrap();
    let ws = kernel
        .open_workspace(std::env::current_dir().unwrap().to_str().unwrap())
        .unwrap();
    let session = kernel.create_session(&ws.id).unwrap();
    let task = kernel.start_task(&session.id, "hello").unwrap();
    assert_eq!(task.status, TaskStatus::Interrupted);
    assert!(task.model_outcome_unknown);
    assert!(
        !serde_json::to_string(&task)
            .unwrap()
            .contains("echoed-sensitive")
    );
    assert!(task.output.unwrap().contains("401"));
    handle.join().unwrap();
}
