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
fn interrupt_replay_falls_back_to_the_legacy_index_and_resumes_after_the_frame() {
    let replies = [
        json!({"protocolVersion":"0.1","serverName":"test","serverVersion":"0",
            "capabilities":{"approvalRunWindow":true}}),
        json!({"startSequence":null,"task":{
            "id":"task-a","sessionId":"session-a","status":"completed","backend":{"type":"model"},
            "inputPreview":"preview","pendingApprovalCount":0,
            "modelOutcomeUnknown":false,"agentOutcomeUnknown":false
        }}),
        json!({"throughSequence":20}),
        json!({"taskId":"task-a","firstSequence":5,"lastSequence":8}),
        json!({"events":[
            {"sequence":5,"taskId":"task-a","data":{"type":"task_started"}},
            {"sequence":8,"taskId":"task-a","data":{"type":"task_completed","text":"Done"}}
        ],"nextAfter":8,"hasMore":false}),
    ];
    let mut frames = Vec::new();
    for (index, result) in replies.into_iter().enumerate() {
        serde_json::to_writer(
            &mut frames,
            &json!({"jsonrpc":"2.0","id":index+1,"result":result}),
        )
        .unwrap();
        frames.push(b'\n');
    }
    let mut client = Client::connect(Cursor::new(frames), Vec::new(), "test", |_| {}).unwrap();
    let (_, mut replay) = client
        .attach_ag_ui_interrupt_replay("session-a", "run", Some("task-a:5:1"), |_| {})
        .unwrap();
    assert_eq!(replay.through(), 20);
    let chunk =
        String::from_utf8(replay.next_chunk(&mut client, |_| {}).unwrap().unwrap()).unwrap();
    assert!(!chunk.contains("RUN_STARTED"));
    assert!(chunk.contains("RUN_FINISHED"));
    assert!(chunk.contains("Done"));
    assert!(replay.next_chunk(&mut client, |_| {}).unwrap().is_none());
    let (_, written) = client.into_parts();
    let sent: Vec<Value> = written
        .split(|b| *b == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_slice(line).unwrap())
        .collect();
    assert_eq!(
        sent.iter()
            .map(|frame| &frame["method"])
            .collect::<Vec<_>>(),
        [
            "initialize",
            "approval/getRun",
            "events/getWatermark",
            "approval/getRunWindow",
            "events/list"
        ]
    );
    assert_eq!(sent[1]["params"], sent[3]["params"]);
    assert_eq!(sent[4]["params"]["after"], 4);
    assert_eq!(sent[4]["params"]["through"], 20);
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
fn artifact_download_resume_rejects_bad_staging_data() {
    use fliuno_protocol::{ArtifactDigest, ArtifactGetParams};
    let identity = ArtifactGetParams {
        task_id: "t".into(),
        artifact_id: "a".into(),
    };
    let digest = ArtifactDigest {
        content_bytes: 3,
        sha256: "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad".into(),
    };
    let mut client = fake_client(b"{\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{\"offset\":2,\"nextOffset\":3,\"contentBytes\":3,\"dataBase64\":\"Yw==\"}}\n");
    let mut oversized = Cursor::new(b"abcd".to_vec());
    assert!(matches!(
        client.resume_artifact_download(&identity, &digest, &mut oversized, |_| {}),
        Err(Error::InvalidArtifactPrefix)
    ));
    assert_eq!(oversized.into_inner(), b"abcd");
    let mut staged = Cursor::new(b"xb".to_vec());
    assert!(matches!(
        client.resume_artifact_download(&identity, &digest, &mut staged, |_| {}),
        Err(Error::ArtifactDigestMismatch)
    ));
    assert_eq!(staged.into_inner(), b"xbc");
    let mut complete = Cursor::new(b"abc".to_vec());
    client
        .resume_artifact_download(&identity, &digest, &mut complete, |_| {})
        .unwrap();
    let (_, written) = client.into_parts();
    let sent = written
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_slice::<Value>(line).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(sent.len(), 2);
    assert_eq!(sent[1]["method"], "artifacts/read");
    assert_eq!(sent[1]["params"]["after"], 2);
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
            after_sequence: None,
            before_sequence: None,
            limit: None,
            scan_limit: None,
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

#[test]
fn usage_summary_batch_checks_positions_watermark_and_duplicate_totals() {
    use fliuno_protocol::{ModelUsageSummaryBatchParams, ModelUsageSummaryScope, ModelUsageTotals};
    let task = ModelUsageSummaryScope::Task {
        task_id: "t1".into(),
    };
    let request = ModelUsageSummaryBatchParams {
        scopes: vec![
            task.clone(),
            ModelUsageSummaryScope::Session {
                session_id: "s1".into(),
            },
            task,
        ],
    };
    let zero = ModelUsageTotals::default();
    let expected = json!([zero, null, zero]);
    let samples = [
        json!({"throughSequence":7,"items":[zero,null]}),
        json!({"throughSequence":u64::MAX,"items":expected}),
        json!({"throughSequence":7,"items":[zero,null,null]}),
        json!({"throughSequence":7,"items":expected}),
    ];
    let replies = samples
        .into_iter()
        .enumerate()
        .map(|(index, result)| {
            let frame = json!({"jsonrpc":"2.0","id":index+2,"result":result});
            format!("{frame}\n")
        })
        .collect::<String>();
    let mut client = fake_client(replies.as_bytes());
    for _ in 0..3 {
        assert!(matches!(
            client.model_usage_summaries(&request, |_| {}),
            Err(Error::InvalidResult)
        ));
    }
    let batch = client.model_usage_summaries(&request, |_| {}).unwrap();
    assert_eq!(batch.through_sequence, 7);
    assert_eq!(batch.items, vec![Some(zero.clone()), None, Some(zero)]);
}

#[test]
fn invalid_task_change_page_preserves_the_replay_cursor() {
    let duplicate = json!({"changes":[{"taskId":"task","sequence":5},{"taskId":"task","sequence":6}],
        "nextAfter":9,"throughSequence":9,"hasMore":false});
    let wrong_watermark = json!({"changes":[],"nextAfter":10,"throughSequence":10,"hasMore":false});
    for reply in [duplicate, wrong_watermark] {
        let frame = json!({"jsonrpc":"2.0","id":2,"result":reply});
        let mut client = fake_client(format!("{frame}\n").as_bytes());
        let mut replay = EventReplay::new(ReplayScope::All, 4, 9).unwrap();
        assert!(matches!(
            replay.next_task_changes(&mut client, 10, |_| {}),
            Err(Error::InvalidReplayPage)
        ));
        assert_eq!(replay.cursor(), 4);
    }
}

#[test]
fn malformed_summary_changes_do_not_move_the_event_cursor() {
    let summary = |id: &str, session: &str| {
        json!({
            "id":id, "sessionId":session, "status":"running", "backend":{"type":"model"},
            "inputPreview":"preview", "pendingApprovalCount":0,
            "modelOutcomeUnknown":false, "agentOutcomeUnknown":false
        })
    };
    for (changes, watermark, next_after) in [
        (
            json!([{"sequence":5,"summary":summary("one", "session")}]),
            8,
            9,
        ),
        (
            json!([{"sequence":5,"summary":summary("one", "foreign")}]),
            10,
            9,
        ),
        (
            json!([{"sequence":5,"summary":summary("one", "session")}]),
            10,
            10,
        ),
        (
            json!([{"sequence":5,"summary":summary("one", "session")},
                {"sequence":6,"summary":summary("one", "session")}]),
            10,
            9,
        ),
        (
            json!([{"sequence":10,"summary":summary("one", "session")}]),
            10,
            9,
        ),
    ] {
        let frame = json!({"jsonrpc":"2.0","id":2,"result":{
            "changes":changes,"nextAfter":next_after,"throughSequence":9,"hasMore":false,
            "summaryThroughSequence":watermark,
        }});
        let mut client = fake_client(format!("{frame}\n").as_bytes());
        let mut replay = EventReplay::new(ReplayScope::Session("session".into()), 4, 9).unwrap();
        assert!(matches!(
            replay.next_task_summary_changes(&mut client, 10, |_| {}),
            Err(Error::InvalidReplayPage)
        ));
        assert_eq!(replay.cursor(), 4);
    }
}

#[test]
fn artifact_catalog_rejects_rewritten_windows_and_invalid_continuations() {
    use fliuno_protocol::{ArtifactCatalogScope, ArtifactCatalogSnapshotParams};
    let item = json!({"id":"a","taskId":"t","mediaType":"text/plain","title":"note","contentBytes":8,"binary":false});
    let changed = |items, after, through, more| json!({"items":items,"nextAfter":after,"throughSequence":through,"hasMore":more});
    let mut wrong_media = item.clone();
    wrong_media["mediaType"] = json!("application/json");
    let replies = [
        changed(json!([item]), 5, 10, false),
        changed(json!([item, item]), 7, 9, true),
        changed(json!([item]), 4, 9, false),
        changed(json!([]), 4, 9, true),
        changed(json!([item]), 9, 9, true),
        changed(json!([wrong_media]), 5, 9, false),
        changed(json!([item]), 5, 9, false),
    ];
    let input = replies
        .iter()
        .enumerate()
        .map(|(i, reply)| format!("{}\n", json!({"jsonrpc":"2.0","id":i+2,"result":reply})))
        .collect::<String>();
    let mut client = fake_client(input.as_bytes());
    let mut catalog = ArtifactCatalog::new(ArtifactCatalogSnapshotParams {
        scope: ArtifactCatalogScope::Session {
            session_id: "session".into(),
        },
        after: 4,
        through_sequence: Some(9),
        limit: 2,
        media_type: Some("text/plain".into()),
    });
    for _ in 0..replies.len() - 1 {
        assert!(matches!(
            catalog.next_page(&mut client, |_| {}),
            Err(Error::InvalidArtifactCatalogPage)
        ));
        let saved = catalog.next_request().unwrap();
        assert_eq!(saved.after, 4);
        assert_eq!(saved.through_sequence, Some(9));
    }
    let page = catalog.next_page(&mut client, |_| {}).unwrap().unwrap();
    assert_eq!(page.next_after, 5);
    assert_eq!(catalog.completed_through(), Some(9));
    assert!(catalog.begin_next_window());
    assert_eq!(catalog.next_request().unwrap().after, 9);
}

#[test]
fn decision_batch_rejects_unbound_and_inconsistent_receipts() {
    use fliuno_protocol::ApprovalDecisionBatchParams;

    let key = json!({"taskId":"task","approvalId":"approval"});
    let request: ApprovalDecisionBatchParams =
        serde_json::from_value(json!({"approvals":[key,key]})).unwrap();
    let receipt =
        json!({"taskId":"task","approvalId":"approval","decision":"allow_once","sequence":7});
    let mut foreign = receipt.clone();
    foreign["approvalId"] = json!("other-approval");
    let bad_replies = [
        json!({"items":[receipt],"throughSequence":9}),
        json!({"items":[foreign,foreign],"throughSequence":9}),
        json!({"items":[receipt,receipt],"throughSequence":6}),
        json!({"items":[receipt,null],"throughSequence":9}),
    ];
    let good_reply = json!({"items":[receipt,receipt],"throughSequence":9});
    let mut frames = Vec::new();
    for (i, result) in bad_replies
        .iter()
        .chain(std::iter::once(&good_reply))
        .enumerate()
    {
        serde_json::to_writer(
            &mut frames,
            &json!({"jsonrpc":"2.0","id":i+2,"result":result}),
        )
        .unwrap();
        frames.push(b'\n');
    }
    let mut client = fake_client(&frames);
    for _ in bad_replies {
        assert!(matches!(
            client.get_approval_decisions(&request, |_| {}),
            Err(Error::InvalidResult)
        ));
    }
    let actual = client.get_approval_decisions(&request, |_| {}).unwrap();
    assert_eq!(serde_json::to_value(actual).unwrap(), good_reply);
}

#[test]
fn history_search_uses_negotiated_scan_limit_without_losing_its_checkpoint() {
    use fliuno_protocol::SessionHistorySearchParams;

    let request = HistorySearchRequest::Session(SessionHistorySearchParams {
        session_id: "session".into(),
        query: "needle".into(),
        role: None,
        through_sequence: None,
        after_sequence: None,
        before_sequence: None,
        limit: None,
        scan_limit: Some(2),
    });
    let mut search = HistorySearch::new(request);
    let initial = serde_json::to_value(search.next_request()).unwrap();
    let mut legacy = fake_client(&[]);
    assert!(matches!(
        search.next_page(&mut legacy, |_| {}),
        Err(Error::UnsupportedHistorySearchScanLimit)
    ));
    assert_eq!(
        serde_json::to_value(search.next_request()).unwrap(),
        initial
    );
    assert_eq!(
        legacy
            .into_parts()
            .1
            .iter()
            .filter(|byte| **byte == b'\n')
            .count(),
        1
    );

    let mut info: Value = serde_json::from_str(HANDSHAKE).unwrap();
    info["result"]["capabilities"]["maxHistorySearchScanLimit"] = json!(512);
    let too_many = json!({"throughSequence":100,"scannedTurns":3,"skippedOversizedTurns":0,"matches":[],"nextBeforeSequence":40});
    let empty = json!({"throughSequence":100,"scannedTurns":2,"skippedOversizedTurns":0,"matches":[],"nextBeforeSequence":40});
    let hit = json!({"throughSequence":100,"scannedTurns":1,"skippedOversizedTurns":0,"matches":[{"taskId":"needle","completionSequence":20}],"nextBeforeSequence":null});
    let mut input = serde_json::to_string(&info).unwrap() + "\n";
    for (i, result) in [too_many, empty, hit].into_iter().enumerate() {
        input += &format!("{}\n", json!({"jsonrpc":"2.0","id":i+2,"result":result}));
    }
    let mut client = Client::connect(
        Cursor::new(input.into_bytes()),
        Vec::new(),
        "scan-limit",
        |_| {},
    )
    .unwrap();
    assert!(matches!(
        search.next_page(&mut client, |_| {}),
        Err(Error::InvalidHistoryPage)
    ));
    assert_eq!(
        serde_json::to_value(search.next_request()).unwrap(),
        initial
    );
    assert!(
        search
            .next_page(&mut client, |_| {})
            .unwrap()
            .unwrap()
            .matches
            .is_empty()
    );
    let checkpoint = serde_json::to_value(search.next_request().unwrap()).unwrap();
    assert_eq!(checkpoint["params"]["scanLimit"], 2);
    assert_eq!(checkpoint["params"]["throughSequence"], 100);
    assert_eq!(checkpoint["params"]["beforeSequence"], 40);
    assert_eq!(
        search
            .next_page(&mut client, |_| {})
            .unwrap()
            .unwrap()
            .matches[0]
            .task_id,
        "needle"
    );
    assert!(search.next_request().is_none());
}

#[test]
fn incremental_history_rejects_legacy_servers_and_out_of_window_pages() {
    use fliuno_protocol::SessionHistorySearchParams;

    let request = HistorySearchRequest::Session(SessionHistorySearchParams {
        session_id: "session".into(),
        query: "needle".into(),
        role: None,
        through_sequence: Some(100),
        after_sequence: Some(20),
        before_sequence: Some(40),
        limit: Some(1),
        scan_limit: None,
    });
    let mut search = HistorySearch::new(request);
    let checkpoint = serde_json::to_value(search.next_request()).unwrap();
    let mut legacy = fake_client(&[]);
    assert!(matches!(
        search.next_page(&mut legacy, |_| {}),
        Err(Error::UnsupportedHistorySearchAfterSequence)
    ));
    assert_eq!(legacy.into_parts().1.split(|&b| b == b'\n').count(), 2);

    let mut info: Value = serde_json::from_str(HANDSHAKE).unwrap();
    info["result"]["capabilities"]["historySearchAfterSequence"] = json!(true);
    let mut input = format!("{info}\n");
    for (id, sequence, next) in [(2, 20, None), (3, 21, Some(21)), (4, 21, None)] {
        let result = json!({"throughSequence":100,"matches":[{"taskId":"hit","completionSequence":sequence}],"scannedTurns":1,"skippedOversizedTurns":0,"nextBeforeSequence":next});
        input += &format!("{}\n", json!({"jsonrpc":"2.0","id":id,"result":result}));
    }
    let mut client = Client::connect(
        Cursor::new(input.into_bytes()),
        Vec::new(),
        "history-window",
        |_| {},
    )
    .unwrap();
    for _ in 0..2 {
        assert!(matches!(
            search.next_page(&mut client, |_| {}),
            Err(Error::InvalidHistoryPage)
        ));
        assert_eq!(
            serde_json::to_value(search.next_request()).unwrap(),
            checkpoint
        );
    }
    let page = search.next_page(&mut client, |_| {}).unwrap().unwrap();
    assert_eq!(page.matches[0].completion_sequence, 21);
    assert!(search.next_request().is_none());
}

#[test]
fn host_ag_ui_model_step_limit_requires_server_support_before_submitting() {
    use crate::ag_ui::{AgUiModelStepLimit, AgUiRunError, AgUiRunInput, AgUiRunOptions};

    assert!(matches!(
        AgUiModelStepLimit::new(0),
        Err(AgUiRunError::ModelStepLimit)
    ));
    assert!(matches!(
        AgUiModelStepLimit::new(9),
        Err(AgUiRunError::ModelStepLimit)
    ));
    let mut client = fake_client(b"");
    let input = AgUiRunInput::parse(br#"{"threadId":"session","runId":"run","messages":[{"id":"msg","role":"user","content":"hello"}]}"#).unwrap();
    let error = client
        .start_ag_ui_text_run(
            "session",
            input,
            AgUiRunOptions {
                model_step_limit: Some(AgUiModelStepLimit::new(1).unwrap()),
                ..AgUiRunOptions::default()
            },
            |_| {},
        )
        .unwrap_err();
    assert!(matches!(error, AgUiRunError::ModelStepLimitUnavailable));
    let (_, sent) = client.into_parts();
    assert_eq!(sent.iter().filter(|&&byte| byte == b'\n').count(), 1);
}
