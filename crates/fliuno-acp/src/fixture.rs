//! Deterministic SDK agent shared by integration tests and the opt-in binary.
use agent_client_protocol::{self as acp, Agent, Client, ConnectTo, v1::schema::*};
use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};
use tokio_util::sync::CancellationToken;

/// Opt-in, deterministic stdio peer for process contract tests in other crates.
#[cfg(feature = "test-fixtures")]
#[allow(dead_code)] // Also compiled directly into the transport-only tests.
pub fn run_stdio() -> Result<(), Box<dyn std::error::Error>> {
    // Process fixtures run concurrently inside bounded Jobs/cgroups. Their
    // scheduler size must not multiply by every CPU exposed by the runner.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;
    runtime.block_on(agent().connect_to(acp::Stdio::new()))?;
    Ok(())
}

pub fn agent() -> impl ConnectTo<Client> {
    let sessions = Arc::new(Mutex::new(HashMap::<SessionId, CancellationToken>::new()));
    let prompt_sessions = Arc::clone(&sessions);
    let next_session = AtomicUsize::new(1);
    let directories = Arc::new(Mutex::new(HashMap::new()));
    let prompt_directories = Arc::clone(&directories);
    let listed_directories = Arc::clone(&directories);
    let loaded_directories = Arc::clone(&directories);
    let closed_directories = Arc::clone(&directories);
    Agent.v1()
        .on_receive_request(
            async |request: InitializeRequest, responder, _cx| {
                // Test fixtures require the adapter to advertise its true scope.
                assert_eq!(request.client_capabilities.fs.read_text_file, request.client_capabilities.fs.write_text_file);
                assert!(!request.client_capabilities.terminal);
                responder.respond(InitializeResponse::new(request.protocol_version).agent_capabilities(
                    AgentCapabilities::new().load_session(true).session_capabilities(
                        SessionCapabilities::new().list(SessionListCapabilities::new()).close(SessionCloseCapabilities::new()),
                    ),
                ))
            },
            acp::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: NewSessionRequest, responder, cx| {
                let id = SessionId::new(format!("session_{}", next_session.fetch_add(1, Ordering::SeqCst)));
                directories.lock().unwrap().insert(id.clone(), request.cwd);
                // Explicitly exercise notifications arriving before the response.
                cx.send_notification(update(&id, "ready"))?;
                responder.respond(NewSessionResponse::new(id))
            },
            acp::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: ListSessionsRequest, responder, _cx| {
                if request.cursor.is_some() {
                    return responder.respond_with_error(acp::Error::invalid_params());
                }
                let mut sessions: Vec<_> = listed_directories.lock().unwrap().iter()
                    .filter(|(_, path)| request.cwd.as_ref().is_none_or(|cwd| cwd == *path))
                    .map(|(id, path)| SessionInfo::new(id.clone(), path.clone()))
                    .collect();
                sessions.sort_unstable_by(|a, b| a.session_id.0.cmp(&b.session_id.0));
                responder.respond(ListSessionsResponse::new(sessions))
            },
            acp::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: LoadSessionRequest, responder, cx| {
                if request.session_id.0.as_ref() != "fixture_saved" || !request.mcp_servers.is_empty()
                    || !request.additional_directories.is_empty()
                {
                    return responder.respond_with_error(acp::Error::invalid_params());
                }
                loaded_directories.lock().unwrap().insert(request.session_id.clone(), request.cwd);
                cx.send_notification(update(&request.session_id, "saved history"))?;
                responder.respond(LoadSessionResponse::new())
            },
            acp::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: CloseSessionRequest, responder, _cx| {
                if closed_directories.lock().unwrap().remove(&request.session_id).is_none() {
                    return responder.respond_with_error(acp::Error::invalid_params());
                }
                responder.respond(CloseSessionResponse::new())
            },
            acp::on_receive_request!(),
        )
        .on_receive_notification(
            async move |request: CancelNotification, _cx| {
                if let Some(cancel) = sessions.lock().unwrap().get(&request.session_id) {
                    cancel.cancel();
                }
                Ok(())
            },
            acp::on_receive_notification!(),
        )
        .on_receive_request(
            async move |request: PromptRequest, responder, cx| {
                let text = match request.prompt.last() {
                    Some(ContentBlock::Text(text)) => text.text.clone(),
                    _ => return responder.respond_with_error(acp::Error::invalid_params()),
                };
                let context = if request.prompt.len() == 2 {
                    match request.prompt.first() {
                        Some(ContentBlock::Text(text)) => Some(text.text.clone()),
                        _ => None,
                    }
                } else { None };
                let cancel = CancellationToken::new();
                let session_id = request.session_id.clone();
                prompt_sessions.lock().unwrap().insert(session_id.clone(), cancel.clone());
                let sessions = Arc::clone(&prompt_sessions);
                let worker = cx.clone();
                let cwd = prompt_directories.lock().unwrap().get(&session_id).cloned().unwrap();
                cx.spawn(async move {
                    match text.as_str() {
                        "context_probe" => {
                            let decoded = context.as_ref().and_then(|text| text.split_once('\n')).and_then(|(_,json)| serde_json::from_str::<serde_json::Value>(json).ok());
                            worker.send_notification(update(&session_id, serde_json::json!({"context":decoded,"contextBytes":context.as_ref().map(String::len),"current":text}).to_string()))?;
                        }
                        _ if text.starts_with("asset ") => {
                            let content = std::fs::read_to_string(cwd.join(&text[6..])).unwrap();
                            worker.send_notification(update(&session_id, content))?;
                        }
                        "error" => return responder.respond_with_error(
                            acp::Error::new(-32099, "synthetic-private-secret")
                                .data(serde_json::json!({"secret":"synthetic-private-secret"})),
                        ),
                        "hang" => {
                            worker.send_notification(update(&session_id, "waiting"))?;
                            let _responder = responder;
                            std::future::pending::<()>().await;
                            return Ok(());
                        }
                        "flood" => {
                            for _ in 0..32 {
                                worker.send_notification(update(&session_id, "flood"))?;
                            }
                        }
                        "oversized" => {
                            worker.send_notification(update(&session_id, "x".repeat(256 * 1024 + 1)))?;
                        }
                        "host_requests" => {
                            let read = worker.send_request(ReadTextFileRequest::new(session_id.clone(), std::env::temp_dir().join("secret.txt"))).block_task().await;
                            let terminal = worker.send_request(CreateTerminalRequest::new(session_id.clone(), "should-never-run")).block_task().await;
                            assert_eq!(read.unwrap_err().code, acp::ErrorCode::METHOD_NOT_FOUND);
                            assert_eq!(terminal.unwrap_err().code, acp::ErrorCode::METHOD_NOT_FOUND);
                            worker.send_notification(update(&session_id, "host requests rejected"))?;
                        }
                        "host_roundtrip" => {
                            let path = cwd.join("via-agent.txt");
                            let write = worker.send_request(WriteTextFileRequest::new(session_id.clone(), path.clone(), "one\n你好\nthree\n")).block_task().await;
                            let read = worker.send_request(ReadTextFileRequest::new(session_id.clone(), path.clone()).line(2).limit(1)).block_task().await;
                            worker.send_notification(update(&session_id, serde_json::json!({
                                "writeOk":write.is_ok(),"read":read.ok().map(|r| r.content),"directRead":std::fs::read(path).is_ok()
                            }).to_string()))?;
                        }
                        "host_write" => {
                            let result = worker.send_request(WriteTextFileRequest::new(session_id.clone(), cwd.join("existing.txt"), "new\n")).block_task().await;
                            worker.send_notification(update(&session_id, format!("writeOk:{}", result.is_ok())))?;
                        }
                        "host_read" => {
                            let result = worker.send_request(ReadTextFileRequest::new(session_id.clone(), cwd.join("input.txt")).line(2).limit(1)).block_task().await;
                            worker.send_notification(update(&session_id, result.map(|r| r.content).unwrap_or_else(|_| "read rejected".into())))?;
                        }
                        "host_burst" => {
                            use futures::StreamExt;
                            let mut replies = futures::stream::FuturesUnordered::new();
                            for _ in 0..16 {
                                replies.push(worker.send_request(ReadTextFileRequest::new(session_id.clone(), cwd.join("input.txt"))).block_task());
                            }
                            for _ in 0..8 { assert!(replies.next().await.unwrap().is_err()); }
                            worker.send_notification(update(&session_id, "host_excess_rejected:8"))?;
                            while let Some(reply) = replies.next().await { assert!(reply.is_ok()); }
                        }
                        _ if text.starts_with("host_escape ") => {
                            let path = std::path::PathBuf::from(&text[12..]);
                            let outside = worker.send_request(ReadTextFileRequest::new(session_id.clone(), path)).block_task().await;
                            let traversal = worker.send_request(WriteTextFileRequest::new(session_id.clone(), cwd.join("../outside.txt"), "escaped")).block_task().await;
                            worker.send_notification(update(&session_id, serde_json::json!({"outsideRejected":outside.is_err(),"traversalRejected":traversal.is_err()}).to_string()))?;
                        }
                        _ if text.starts_with("probe ") => {
                            let input: serde_json::Value = serde_json::from_str(&text[6..]).unwrap();
                            let path = input["path"].as_str().unwrap();
                            let address = input["address"].as_str().unwrap().parse().unwrap();
                            let result = serde_json::json!({
                                "appContainer": fliuno_sandbox::current_process_is_app_container().unwrap_or(false),
                                "hostRead": std::fs::read(path).is_ok(),
                                "hostWrite": std::fs::write(path, "modified").is_ok(),
                                "hostTcp": std::net::TcpStream::connect_timeout(&address, std::time::Duration::from_millis(300)).is_ok(),
                                "scratchWrite": std::fs::write("probe.txt", "private").is_ok(),
                            });
                            worker.send_notification(update(&session_id, result.to_string()))?;
                        }
                        "permission" | "wait_cancel" | "persistent" => {
                            let options = if text == "persistent" {
                                vec![PermissionOption::new("always", "Always", PermissionOptionKind::AllowAlways)]
                            } else {
                                // Names and positions deliberately contradict kinds.
                                vec![
                                    PermissionOption::new("always", "Allow once", PermissionOptionKind::AllowAlways),
                                    PermissionOption::new("reject", "Allow", PermissionOptionKind::RejectOnce),
                                    PermissionOption::new("once", "Never", PermissionOptionKind::AllowOnce),
                                ]
                            };
                            let result = worker.send_request(RequestPermissionRequest::new(
                                session_id.clone(),
                                ToolCallUpdate::new("tool_1", ToolCallUpdateFields::new().title("Review operation")),
                                options,
                            )).block_task().await?;
                            let outcome = serde_json::to_string(&result.outcome).unwrap();
                            worker.send_notification(update(&session_id, outcome))?;
                            if text == "wait_cancel" {
                                cancel.cancelled().await;
                            }
                        }
                        "permissions_burst" => {
                            use futures::StreamExt;
                            let mut replies = futures::stream::FuturesUnordered::new();
                            for index in 0..32 {
                                replies.push(worker.send_request(RequestPermissionRequest::new(
                                    session_id.clone(),
                                    ToolCallUpdate::new(format!("tool_{index}"), ToolCallUpdateFields::new()),
                                    vec![PermissionOption::new("reject", "Reject", PermissionOptionKind::RejectOnce)],
                                )).block_task());
                            }
                            // The first 16 have no UI response yet. Excess work
                            // must complete as cancelled rather than spawn waits.
                            for _ in 0..16 {
                                assert_eq!(replies.next().await.unwrap()?.outcome, RequestPermissionOutcome::Cancelled);
                            }
                            worker.send_notification(update(&session_id, "excess_cancelled:16"))?;
                            while let Some(reply) = replies.next().await {
                                assert_eq!(reply?.outcome, RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new("reject")));
                            }
                        }
                        _ => {
                            worker.send_notification(update(&session_id, "你好，"))?;
                            worker.send_notification(update(&session_id, &text))?;
                        }
                    }
                    let reason = if cancel.is_cancelled() { StopReason::Cancelled }
                        else if text == "max_tokens" { StopReason::MaxTokens }
                        else { StopReason::EndTurn };
                    sessions.lock().unwrap().remove(&session_id);
                    responder.respond(PromptResponse::new(reason))
                })
            },
            acp::on_receive_request!(),
        )
}

fn update(session_id: &SessionId, text: impl Into<String>) -> SessionNotification {
    SessionNotification::new(
        session_id.clone(),
        SessionUpdate::AgentMessageChunk(ContentChunk::new(ContentBlock::Text(TextContent::new(
            text,
        )))),
    )
}
