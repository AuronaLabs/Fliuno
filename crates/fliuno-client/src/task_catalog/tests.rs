use super::*;
use serde_json::{Value, json};
use std::io::Cursor;

fn summary(id: &str, session: &str, status: &str) -> Value {
    json!({"id":id,"sessionId":session,"status":status,"backend":{"type":"model"},
        "inputPreview":"","pendingApprovalCount":0,"modelOutcomeUnknown":false,"agentOutcomeUnknown":false})
}

fn reply(items: &[Value], next_after: u64, has_more: bool) -> Value {
    json!({"items":items,"nextAfter":next_after,"hasMore":has_more})
}

#[test]
fn invalid_replies_do_not_advance_the_saved_query() {
    let first = summary("t1", "s1", "completed");
    let second = summary("t2", "s1", "completed");
    let wrong_status = summary("t2", "s1", "running");
    let other_session = summary("t2", "s2", "completed");
    let handshake =
        json!({"protocolVersion":"0.1","serverName":"test","serverVersion":"0","capabilities":{}});
    let responses = [
        handshake,
        reply(&[], 8, true),
        reply(&[first.clone(), second.clone()], 8, true),
        reply(&[first.clone(), first.clone()], 7, true),
        reply(&[first.clone(), wrong_status], 7, true),
        reply(&[first.clone(), other_session], 7, true),
        reply(std::slice::from_ref(&first), 7, true),
        reply(&[second, first], 7, true),
    ];
    let mut source = Vec::new();
    for (i, result) in responses.into_iter().enumerate() {
        writeln!(
            source,
            "{}",
            json!({"jsonrpc":"2.0","id":i+1,"result":result})
        )
        .unwrap();
    }
    let mut client = Client::connect(Cursor::new(source), Vec::new(), "catalog", |_| {}).unwrap();
    let mut catalog =
        TaskSummaryCatalog::new(TaskSummaryCatalogRequest::Session(TaskSummaryListParams {
            session_id: "s1".into(),
            status: Some(fliuno_protocol::TaskStatus::Completed),
            order: Some(TaskSummaryOrder::NewestFirst),
            after: 8,
            limit: 2,
            target_bytes: None,
        }));
    for _ in 0..6 {
        assert!(matches!(
            catalog.next_page(&mut client, |_| {}),
            Err(Error::InvalidTaskSummaryPage)
        ));
        assert_eq!(
            serde_json::to_value(catalog.next_request()).unwrap()["params"]["after"],
            8
        );
    }
    assert_eq!(
        catalog
            .next_page(&mut client, |_| {})
            .unwrap()
            .unwrap()
            .next_after,
        7
    );
    assert_eq!(
        serde_json::to_value(catalog.next_request()).unwrap()["params"]["after"],
        7
    );
}
