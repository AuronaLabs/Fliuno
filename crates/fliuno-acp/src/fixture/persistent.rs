//! A real SDK fixture whose saved ACP sessions outlive its stdio process.
use super::*;
use std::{
    fs,
    path::{Path, PathBuf},
};

fn state_file(cwd: &Path, id: &SessionId) -> PathBuf {
    assert!(uuid::Uuid::parse_str(&id.0).is_ok());
    cwd.join(format!("fixture-{}.json", id.0))
}

pub(super) fn agent() -> impl ConnectTo<Client> {
    let active = Arc::new(Mutex::new(HashMap::<SessionId, PathBuf>::new()));
    let creating = Arc::clone(&active);
    let resuming = Arc::clone(&active);
    Agent
        .v1()
        .on_receive_request(
            async move |request: InitializeRequest, responder, _cx| {
                assert!(!request.client_capabilities.fs.read_text_file);
                assert!(!request.client_capabilities.fs.write_text_file);
                responder.respond(
                    InitializeResponse::new(request.protocol_version).agent_capabilities(
                        AgentCapabilities::new().session_capabilities(
                            SessionCapabilities::new()
                                .resume(SessionResumeCapabilities::new())
                                .list(SessionListCapabilities::new()),
                        ),
                    ),
                )
            },
            acp::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: ListSessionsRequest, responder, _cx| {
                let cwd = request.cwd.expect("catalog must have a configured filter");
                assert_eq!(
                    cwd,
                    std::env::current_dir().unwrap().canonicalize().unwrap()
                );
                let index = match request.cursor.as_deref() {
                    None => 0,
                    Some("next-page") => 1,
                    Some("hold") => {
                        fs::write(cwd.join("catalog-started"), b"").unwrap();
                        while !cwd.join("catalog-release").is_file() {
                            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                        }
                        0
                    }
                    Some("oversize") => {
                        let item = SessionInfo::new("too-large", cwd).title("x".repeat(256 * 1024));
                        return responder.respond(ListSessionsResponse::new(vec![item]));
                    }
                    _ => {
                        return responder.respond_with_error(acp::Error::new(
                            -32091,
                            "fixture-private-catalog-error",
                        ));
                    }
                };
                let mut ids: Vec<_> = fs::read_dir(&cwd)
                    .unwrap()
                    .filter_map(|entry| {
                        let name = entry.ok()?.file_name().into_string().ok()?;
                        let id = name.strip_prefix("fixture-")?.strip_suffix(".json")?;
                        uuid::Uuid::parse_str(id).ok()
                    })
                    .collect();
                ids.sort_unstable();
                let page = ids
                    .get(index)
                    .map(|id| SessionInfo::new(id.to_string(), cwd).title("Saved fixture session"));
                responder.respond(
                    ListSessionsResponse::new(page.into_iter().collect())
                        .next_cursor((index == 0 && ids.len() > 1).then(|| "next-page".to_owned())),
                )
            },
            acp::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: NewSessionRequest, responder, _cx| {
                let id = SessionId::new(uuid::Uuid::new_v4().to_string());
                let cwd = request.cwd;
                fs::write(state_file(&cwd, &id), b"[]").unwrap();
                creating.lock().unwrap().insert(id.clone(), cwd);
                responder.respond(NewSessionResponse::new(id))
            },
            acp::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: ResumeSessionRequest, responder, _cx| {
                let path = state_file(&request.cwd, &request.session_id);
                let saved = fs::read(&path)
                    .ok()
                    .and_then(|body| serde_json::from_slice::<Vec<String>>(&body).ok());
                if saved.is_none_or(|turns| turns.is_empty() || turns[0] == "refuse_resume") {
                    return responder.respond_with_error(acp::Error::invalid_params());
                }
                assert!(request.mcp_servers.is_empty());
                assert!(request.additional_directories.is_empty());
                resuming
                    .lock()
                    .unwrap()
                    .insert(request.session_id, request.cwd);
                responder.respond(ResumeSessionResponse::new())
            },
            acp::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: PromptRequest, responder, cx| {
                assert_eq!(
                    request.prompt.len(),
                    1,
                    "the existing session owns its history"
                );
                let session_id = request.session_id;
                let cwd = active.lock().unwrap().get(&session_id).cloned().unwrap();
                let [ContentBlock::Text(text)] = request.prompt.as_slice() else {
                    return responder.respond_with_error(acp::Error::invalid_params());
                };
                let path = state_file(&cwd, &session_id);
                let mut turns: Vec<String> =
                    serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
                turns.push(text.text.clone());
                fs::write(path, serde_json::to_vec(&turns).unwrap()).unwrap();
                if text.text == "lose_response" {
                    cx.send_notification(update(&session_id, "prompt stored"))?;
                    let _pending_response = responder;
                    std::future::pending::<()>().await;
                    return Ok(());
                }
                cx.send_notification(update(&session_id, turns.join(" | ")))?;
                responder.respond(PromptResponse::new(StopReason::EndTurn))
            },
            acp::on_receive_request!(),
        )
}
