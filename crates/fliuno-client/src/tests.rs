use super::*;
use serde_json::json;
use std::io::Cursor;

const HANDSHAKE: &str = "{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"protocolVersion\":\"0.1\",\"serverName\":\"test\",\"serverVersion\":\"0\",\"capabilities\":{}}}\n";

fn fake_client(additional: &[u8]) -> Client<Cursor<Vec<u8>>, Vec<u8>> {
    let mut bytes = HANDSHAKE.as_bytes().to_vec();
    bytes.extend_from_slice(additional);
    Client::connect(Cursor::new(bytes), Vec::new(), "test", |_| {}).unwrap()
}

#[test]
fn local_request_limit_does_not_write_or_consume_a_correlation_id() {
    let mut client = fake_client(b"{\"jsonrpc\":\"2.0\",\"id\":2,\"result\":true}\n");
    let large = json!({"payload":"x".repeat(fliuno_protocol::MAX_APP_REQUEST_BYTES)});
    let rejected: Result<bool, _> = client.call("probe", &large, |_| {});
    assert!(matches!(rejected, Err(Error::RequestTooLarge)));
    assert!(client.call::<_, bool>("probe", &json!({}), |_| {}).unwrap());
    let (_, written) = client.into_parts();
    let sent: Vec<Value> = written
        .split(|b| *b == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_slice(line).unwrap())
        .collect();
    assert_eq!(sent.len(), 2);
    assert_eq!(sent[1]["id"], 2);
}

#[test]
fn incorrect_correlation_retires_the_connection_without_a_second_send() {
    let mut client = fake_client(b"{\"jsonrpc\":\"2.0\",\"id\":3,\"result\":true}\n");
    let mismatched: Result<bool, _> = client.call("probe", &json!({}), |_| {});
    assert!(matches!(mismatched, Err(Error::InvalidFrame(_))));
    assert!(matches!(
        client.call::<_, bool>("probe", &json!({}), |_| {}),
        Err(Error::Closed)
    ));
    let (_, written) = client.into_parts();
    assert_eq!(written.iter().filter(|b| **b == b'\n').count(), 2);
}

#[test]
fn oversize_frame_retires_the_connection_without_draining_more_bytes() {
    let oversized = vec![b'x'; DEFAULT_RESPONSE_BYTES + 1];
    let mut client = fake_client(&oversized);
    assert!(matches!(
        client.read_notification(),
        Err(Error::ResponseTooLarge)
    ));
    assert!(matches!(client.read_notification(), Err(Error::Closed)));
    let (reader, _) = client.into_parts();
    assert_eq!(reader.position(), HANDSHAKE.len() as u64);
}

#[test]
fn replay_rejects_an_inconsistent_page_and_advances_across_an_empty_scoped_tail() {
    let mut client = fake_client(concat!(
        "{\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{\"events\":[],\"nextAfter\":4,\"hasMore\":true}}\n",
        "{\"jsonrpc\":\"2.0\",\"id\":3,\"result\":{\"events\":[],\"nextAfter\":4,\"hasMore\":false}}\n",
    ).as_bytes());
    let mut replay = EventReplay::new(ReplayScope::Workspace("workspace".into()), 4, 10).unwrap();
    assert!(matches!(
        replay.next_page(&mut client, 1, None, |_| {}),
        Err(Error::InvalidReplayPage)
    ));
    assert_eq!(replay.cursor(), 4);
    assert!(
        replay
            .next_page(&mut client, 1, None, |_| {})
            .unwrap()
            .is_none()
    );
    assert_eq!(replay.cursor(), 10);
    assert!(
        replay
            .next_page(&mut client, 1, None, |_| {})
            .unwrap()
            .is_none()
    );
    let (_, written) = client.into_parts();
    let sent: Vec<Value> = written
        .split(|b| *b == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_slice(line).unwrap())
        .collect();
    assert_eq!(sent.len(), 3);
    assert_eq!(sent[1]["method"], "events/listWorkspace");
    assert_eq!(sent[1]["params"], sent[2]["params"]);
    assert_eq!(sent[2]["params"]["through"], 10);
}

#[test]
fn borrowed_envelopes_preserve_null_results_and_reject_conflicting_fields() {
    let mut client = fake_client(
        concat!(
            "{\"jsonrpc\":\"2.0\",\"method\":\"test/notice\",\"params\":null}\n",
            "{\"jsonrpc\":\"2.0\",\"id\":2,\"result\":null}\n",
            "{\"jsonrpc\":\"2.0\",\"id\":3,\"result\":null,\"error\":null}\n",
        )
        .as_bytes(),
    );
    let notification = client.read_notification().unwrap();
    assert_eq!(notification.method, "test/notice");
    assert_eq!(notification.params, Value::Null);
    let result: Option<u32> = client.call("probe", &json!({}), |_| {}).unwrap();
    assert_eq!(result, None);
    assert!(matches!(
        client.call::<_, Value>("probe", &json!({}), |_| {}),
        Err(Error::InvalidFrame(_))
    ));
    assert!(matches!(client.read_notification(), Err(Error::Closed)));
}

#[test]
fn artifact_download_rejects_malformed_ranges_before_writing_and_keeps_connection() {
    use fliuno_protocol::{ArtifactDigest, ArtifactGetParams};
    let first = json!({"offset":0,"nextOffset":2,"contentBytes":2,"dataBase64":"AAE="});
    for (field, invalid) in [
        ("offset", json!(1)),
        ("nextOffset", json!(0)),
        ("contentBytes", json!(3)),
        ("dataBase64", json!("!!!!")),
        ("dataBase64", json!("AA==")),
    ] {
        let mut chunk = first.clone();
        chunk[field] = invalid;
        let frames = format!(
            "{}\n{{\"jsonrpc\":\"2.0\",\"id\":3,\"result\":true}}\n",
            json!({"jsonrpc":"2.0","id":2,"result":chunk})
        );
        let mut client = fake_client(frames.as_bytes());
        let key = ArtifactGetParams {
            task_id: "t".into(),
            artifact_id: "a".into(),
        };
        let digest = ArtifactDigest {
            content_bytes: 2,
            sha256: "0".repeat(64),
        };
        let mut output = Vec::new();
        assert!(
            matches!(
                client.download_artifact(&key, &digest, &mut output, |_| {}),
                Err(Error::InvalidArtifactChunk)
            ),
            "{field}"
        );
        assert!(output.is_empty());
        assert!(client.call::<_, bool>("probe", &json!({}), |_| {}).unwrap());
    }
}

#[test]
fn artifact_download_checks_empty_body_against_pinned_digest() {
    use fliuno_protocol::{ArtifactDigest, ArtifactGetParams};
    let mut client = fake_client(concat!(
        "{\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{\"offset\":0,\"nextOffset\":0,\"contentBytes\":0,\"dataBase64\":\"\"}}\n",
        "{\"jsonrpc\":\"2.0\",\"id\":3,\"result\":{\"offset\":0,\"nextOffset\":0,\"contentBytes\":0,\"dataBase64\":\"\"}}\n"
    ).as_bytes());
    let key = ArtifactGetParams {
        task_id: "t".into(),
        artifact_id: "a".into(),
    };
    let mut digest = ArtifactDigest {
        content_bytes: 0,
        sha256: "not-a-hash".into(),
    };
    let mut output = Vec::new();
    assert!(matches!(
        client.download_artifact(&key, &digest, &mut output, |_| {}),
        Err(Error::InvalidArtifactDigest)
    ));
    digest.sha256 = "0".repeat(64);
    assert!(matches!(
        client.download_artifact(&key, &digest, &mut output, |_| {}),
        Err(Error::ArtifactDigestMismatch)
    ));
    digest.sha256 = "E3B0C44298FC1C149AFBF4C8996FB92427AE41E4649B934CA495991B7852B855".into();
    client
        .download_artifact(&key, &digest, &mut output, |_| {})
        .unwrap();
    assert!(output.is_empty());
}

#[test]
fn history_checkpoint_retries_invalid_pages_without_skipping_entries() {
    use fliuno_protocol::SessionHistoryListParams;

    let mut client = fake_client(concat!(
        "{\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{\"throughSequence\":31,\"entries\":[{\"kind\":\"oversized\",\"taskId\":\"new\",\"completionSequence\":25}],\"nextBeforeSequence\":25}}\n",
        "{\"jsonrpc\":\"2.0\",\"id\":3,\"result\":{\"throughSequence\":32,\"entries\":[{\"kind\":\"oversized\",\"taskId\":\"old\",\"completionSequence\":10}],\"nextBeforeSequence\":null}}\n",
        "{\"jsonrpc\":\"2.0\",\"id\":4,\"result\":{\"throughSequence\":31,\"entries\":[{\"kind\":\"oversized\",\"taskId\":\"new\",\"completionSequence\":25}],\"nextBeforeSequence\":null}}\n",
        "{\"jsonrpc\":\"2.0\",\"id\":5,\"result\":{\"throughSequence\":31,\"entries\":[{\"kind\":\"oversized\",\"taskId\":\"old\",\"completionSequence\":10}],\"nextBeforeSequence\":null}}\n",
    ).as_bytes());
    let mut history = SessionHistory::new(SessionHistoryListParams {
        session_id: "session".into(),
        through_sequence: None,
        before_sequence: None,
        limit: Some(1),
        max_bytes: Some(1024),
    });
    assert_eq!(
        history
            .next_page(&mut client, |_| {})
            .unwrap()
            .unwrap()
            .through_sequence,
        31
    );
    let saved = history.next_request().unwrap().clone();
    assert_eq!(saved.through_sequence, Some(31));
    assert_eq!(saved.before_sequence, Some(25));
    let mut resumed = SessionHistory::new(saved);
    for _ in 0..2 {
        assert!(matches!(
            resumed.next_page(&mut client, |_| {}),
            Err(Error::InvalidHistoryPage)
        ));
        assert_eq!(resumed.next_request().unwrap().before_sequence, Some(25));
        assert_eq!(resumed.through(), Some(31));
    }
    assert!(resumed.next_page(&mut client, |_| {}).unwrap().is_some());
    assert!(resumed.next_request().is_none());
    assert!(resumed.next_page(&mut client, |_| {}).unwrap().is_none());
    let (_, written) = client.into_parts();
    let sent: Vec<Value> = written
        .split(|b| *b == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_slice(line).unwrap())
        .collect();
    assert_eq!(sent.len(), 5);
    assert_eq!(sent[2]["params"], sent[3]["params"]);
    assert_eq!(sent[3]["params"], sent[4]["params"]);
}

#[test]
fn event_feed_coalesces_notices_and_recovers_from_the_delivered_cursor() {
    let mut client = fake_client(concat!(
        "{\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{\"throughSequence\":8}}\n",
        "{\"jsonrpc\":\"2.0\",\"id\":3,\"result\":{\"events\":[{\"sequence\":6,\"taskId\":\"task\",\"data\":{\"type\":\"task_started\"}}],\"nextAfter\":6,\"hasMore\":false}}\n",
        "{\"jsonrpc\":\"2.0\",\"method\":\"notifications/event\",\"params\":{\"event\":{\"sequence\":14}}}\n",
        "{\"jsonrpc\":\"2.0\",\"method\":\"notifications/eventGap\",\"params\":{\"afterSequence\":14}}\n",
        "{\"jsonrpc\":\"2.0\",\"id\":4,\"result\":{\"events\":[{\"sequence\":12,\"taskId\":\"task\",\"data\":{\"type\":\"task_interrupted\"}}],\"nextAfter\":12,\"hasMore\":false}}\n",
        "{\"jsonrpc\":\"2.0\",\"method\":\"notifications/eventGap\",\"params\":{\"afterSequence\":14}}\n",
        "{\"jsonrpc\":\"2.0\",\"method\":\"other\",\"params\":{}}\n",
        "{\"jsonrpc\":\"2.0\",\"id\":5,\"result\":{\"throughSequence\":18}}\n",
        "{\"jsonrpc\":\"2.0\",\"id\":6,\"result\":{\"events\":[{\"sequence\":17,\"taskId\":\"task\",\"data\":{\"type\":\"task_resumed\"}}],\"nextAfter\":17,\"hasMore\":false}}\n",
    ).as_bytes());
    let mut feed =
        EventFeed::subscribe(&mut client, ReplayScope::Task("task".into()), 5, |_| {}).unwrap();
    let first = feed
        .next_page(&mut client, 10, None, |_| {})
        .unwrap()
        .unwrap();
    assert_eq!(first[0].sequence, 6);
    assert_eq!(feed.cursor(), 8);
    for sequence in [9, 12, 12, 11] {
        assert!(
            feed.observe(Notification {
                method: "notifications/event".into(),
                params: json!({"event":{"sequence":sequence}}),
            })
            .is_none()
        );
    }
    let live = feed
        .next_page(&mut client, 10, None, |_| {})
        .unwrap()
        .unwrap();
    assert_eq!(live[0].sequence, 12);
    let mut other = Vec::new();
    let repaired = feed
        .next_page(&mut client, 10, None, |note| other.push(note.method))
        .unwrap()
        .unwrap();
    assert_eq!(repaired[0].sequence, 17);
    assert_eq!(feed.cursor(), 18);
    assert_eq!(other, vec!["other"]);
    assert!(
        feed.next_page(&mut client, 10, None, |_| {})
            .unwrap()
            .is_none()
    );
    let (_, written) = client.into_parts();
    let sent: Vec<Value> = written
        .split(|b| *b == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_slice(line).unwrap())
        .collect();
    assert_eq!(sent.len(), 6);
    assert_eq!(sent[3]["params"]["through"], 12);
    assert_eq!(sent[4]["method"], "events/setScope");
    assert_eq!(sent[5]["params"]["after"], 12);
}

#[test]
fn ag_ui_run_rejects_unsupported_authority_before_sending_a_request() {
    use crate::ag_ui::AgUiRunError;
    let mut client = fake_client(&[]);
    let body = json!({"threadId":"chosen-session","runId":"chosen-run",
        "messages":[{"id":"u1","role":"user","content":"hello"}]});
    for (field, value) in [
        (
            "tools",
            json!([{"name":"remote-tool","description":"not registered"}]),
        ),
        (
            "context",
            json!([{"description":"client context","value":"override"}]),
        ),
        ("parentRunId", json!("outside-run")),
        ("resume", json!([{"interruptId":"old","status":"resolved"}])),
        ("state", json!({"unexpected": true})),
    ] {
        let mut changed = body.clone();
        changed[field] = value;
        assert!(matches!(
            client.start_ag_ui_run(
                "chosen-session",
                &serde_json::to_vec(&changed).unwrap(),
                |_| {}
            ),
            Err(AgUiRunError::UnsupportedInput)
        ));
    }
    let mut wrong_role = body.clone();
    wrong_role["messages"][0]["role"] = json!("assistant");
    assert!(matches!(
        client.start_ag_ui_run(
            "chosen-session",
            &serde_json::to_vec(&wrong_role).unwrap(),
            |_| {}
        ),
        Err(AgUiRunError::UserText)
    ));
    let mut encrypted = body;
    encrypted["messages"][0]["encryptedValue"] = json!("encrypted");
    assert!(matches!(
        client.start_ag_ui_run(
            "chosen-session",
            &serde_json::to_vec(&encrypted).unwrap(),
            |_| {}
        ),
        Err(AgUiRunError::UnsupportedInput)
    ));
    let (_, written) = client.into_parts();
    assert_eq!(written.iter().filter(|&&byte| byte == b'\n').count(), 1);
}

#[test]
fn history_search_resumes_an_empty_window_and_retries_the_same_cursor() {
    use fliuno_protocol::{MAX_HISTORY_SEARCH_TURNS, WorkspaceHistorySearchParams};

    let replies = [
        json!({"throughSequence":500,"matches":[],"scannedTurns":MAX_HISTORY_SEARCH_TURNS,"skippedOversizedTurns":4,"nextBeforeSequence":300}),
        json!({"throughSequence":501,"matches":[],"scannedTurns":MAX_HISTORY_SEARCH_TURNS,"skippedOversizedTurns":4,"nextBeforeSequence":150}),
        json!({"throughSequence":500,"matches":[{"taskId":"needle","sessionId":"session","completionSequence":220}],"scannedTurns":1,"skippedOversizedTurns":0,"nextBeforeSequence":null}),
    ];
    let mut wire = Vec::new();
    for (index, page) in replies.into_iter().enumerate() {
        serde_json::to_writer(
            &mut wire,
            &json!({"jsonrpc":"2.0","id":index+2,"result":page}),
        )
        .unwrap();
        wire.push(b'\n');
    }
    let mut client = fake_client(&wire);
    let mut search = HistorySearch::new(HistorySearchRequest::Workspace(
        WorkspaceHistorySearchParams {
            workspace_id: "workspace".into(),
            query: "needle".into(),
            role: None,
            through_sequence: None,
            before_sequence: None,
            limit: None,
        },
    ));
    let empty = search.next_page(&mut client, |_| {}).unwrap().unwrap();
    assert!(empty.matches.is_empty());
    let checkpoint = serde_json::to_value(search.next_request().unwrap()).unwrap();
    assert_eq!(checkpoint["params"]["throughSequence"], 500);
    assert_eq!(checkpoint["params"]["beforeSequence"], 300);
    assert!(matches!(
        search.next_page(&mut client, |_| {}),
        Err(Error::InvalidHistoryPage)
    ));
    assert_eq!(
        serde_json::to_value(search.next_request().unwrap()).unwrap(),
        checkpoint
    );
    let hit = search.next_page(&mut client, |_| {}).unwrap().unwrap();
    assert_eq!(hit.matches[0].task_id, "needle");
    assert!(search.next_page(&mut client, |_| {}).unwrap().is_none());
    assert!(search.next_request().is_none());
    let (_, written) = client.into_parts();
    let sent: Vec<Value> = written
        .split(|b| *b == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_slice(line).unwrap())
        .collect();
    assert_eq!(sent.len(), 4);
    assert_eq!(sent[2]["params"], checkpoint["params"]);
    assert_eq!(sent[2]["params"], sent[3]["params"]);
}

#[test]
fn model_usage_replay_rejects_unfiltered_events_before_advancing() {
    let mut client = fake_client(concat!(
        "{\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{\"events\":[{\"sequence\":5,\"taskId\":\"t\",\"data\":{\"type\":\"model_started\",\"step\":1}}],\"nextAfter\":5,\"hasMore\":false}}\n",
        "{\"jsonrpc\":\"2.0\",\"id\":3,\"result\":{\"events\":[{\"sequence\":6,\"taskId\":\"t\",\"data\":{\"type\":\"model_usage_reported\",\"step\":1,\"usage\":{\"provider\":\"fixture\",\"inputTokens\":3,\"outputTokens\":2}}}],\"nextAfter\":6,\"hasMore\":false}}\n"
    ).as_bytes());
    let mut replay =
        EventReplay::for_model_usage(ModelUsageScope::Workspace("workspace".into()), 4, 9).unwrap();
    assert!(matches!(
        replay.next_page(&mut client, 1, None, |_| {}),
        Err(Error::InvalidReplayPage)
    ));
    assert_eq!(replay.cursor(), 4);
    let page = replay
        .next_page(&mut client, 1, None, |_| {})
        .unwrap()
        .unwrap();
    assert_eq!(page[0].sequence, 6);
    assert_eq!(replay.cursor(), 9);
    assert!(
        replay
            .next_page(&mut client, 1, None, |_| {})
            .unwrap()
            .is_none()
    );
    let (_, written) = client.into_parts();
    let calls: Vec<Value> = written
        .split(|b| *b == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_slice(line).unwrap())
        .collect();
    assert_eq!(calls.len(), 3);
    assert_eq!(calls[1]["method"], "events/listWorkspaceModelUsage");
    assert_eq!(calls[2]["params"], calls[1]["params"]);
    assert_eq!(calls[2]["params"]["through"], 9);
}

#[test]
fn approval_inbox_preserves_the_checkpoint_when_the_snapshot_changes() {
    use fliuno_protocol::{ApprovalInboxListParams, ApprovalListParams};
    let item = json!({"id":"approve","taskId":"task","sessionId":"session","kind":"tool_call","allowOnceAvailable":true,"expiresAtUnixMs":null});
    let stamp = json!({"throughSequence":41,"capturedAtUnixMs":9001});
    let replies = [
        json!({"snapshot":stamp,"page":{"items":[item],"nextAfter":4,"hasMore":true}}),
        json!({"snapshot":{"throughSequence":41,"capturedAtUnixMs":9002},"page":{"items":[],"nextAfter":4,"hasMore":false}}),
        json!({"snapshot":stamp,"page":{"items":[],"nextAfter":4,"hasMore":false}}),
    ];
    let mut wire = Vec::new();
    for (index, result) in replies.into_iter().enumerate() {
        serde_json::to_writer(
            &mut wire,
            &json!({"jsonrpc":"2.0","id":index+2,"result":result}),
        )
        .unwrap();
        wire.push(b'\n');
    }
    let mut client = fake_client(&wire);
    let mut inbox = ApprovalInbox::new(ApprovalInboxListParams {
        page: ApprovalListParams {
            workspace_id: None,
            session_id: Some("session".into()),
            after: 0,
            limit: 1,
            target_bytes: None,
        },
        snapshot: None,
    });
    let first = inbox.next_page(&mut client, |_| {}).unwrap().unwrap();
    assert_eq!(first.page.items[0].id, "approve");
    let saved = serde_json::to_value(inbox.next_request().unwrap()).unwrap();
    assert_eq!(saved["snapshot"], stamp);
    assert_eq!(saved["page"]["after"], 4);
    let mut resumed = ApprovalInbox::new(serde_json::from_value(saved.clone()).unwrap());
    assert!(matches!(
        resumed.next_page(&mut client, |_| {}),
        Err(Error::InvalidApprovalInboxPage)
    ));
    assert_eq!(
        serde_json::to_value(resumed.next_request().unwrap()).unwrap(),
        saved
    );
    assert!(
        resumed
            .next_page(&mut client, |_| {})
            .unwrap()
            .unwrap()
            .page
            .items
            .is_empty()
    );
    assert!(resumed.next_page(&mut client, |_| {}).unwrap().is_none());
}

#[test]
fn task_summary_batch_rejects_shifted_or_foreign_response_items() {
    let summary = |id: &str, session: &str| {
        json!({
            "id":id, "sessionId":session, "status":"running", "backend":{"type":"model"},
            "inputPreview":"preview", "pendingApprovalCount":0,
            "modelOutcomeUnknown":false, "agentOutcomeUnknown":false
        })
    };
    let request = fliuno_protocol::TaskSummaryBatchParams {
        task_ids: vec!["first".into(), "second".into()],
        workspace_id: None,
        session_id: Some("session".into()),
    };
    for items in [
        json!([summary("second", "session"), null]),
        json!([null, summary("second", "foreign")]),
        json!([null]),
    ] {
        let frame = json!({"jsonrpc":"2.0","id":2,"result":{"throughSequence":7,"items":items}});
        let mut client = fake_client(format!("{frame}\n").as_bytes());
        assert!(matches!(
            client.get_task_summaries(&request, |_| {}),
            Err(Error::InvalidResult)
        ));
    }
}
