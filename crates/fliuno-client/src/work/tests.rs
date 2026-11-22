use super::*;
use serde_json::{Value, json};
use std::io::Cursor;

fn connect(
    supports_preview: bool,
    replies: impl IntoIterator<Item = Value>,
) -> Client<Cursor<Vec<u8>>, Vec<u8>> {
    let mut input = Vec::new();
    let handshake = json!({"protocolVersion":"0.1","serverName":"test","serverVersion":"0",
        "capabilities": if supports_preview { json!({"workRecentTasks":true}) } else { json!({}) }});
    for (i, result) in std::iter::once(handshake).chain(replies).enumerate() {
        writeln!(
            input,
            "{}",
            json!({"jsonrpc":"2.0","id":i+1,"result":result})
        )
        .unwrap();
    }
    Client::connect(Cursor::new(input), Vec::new(), "work-test", |_| {}).unwrap()
}

fn request() -> WorkSnapshotParams {
    WorkSnapshotParams {
        session_id: Some("s1".into()),
        workspace_id: None,
        approval_limit: 1,
        approval_target_bytes: None,
        recent_task_limit: Some(2),
        include_actions: false,
    }
}

fn snapshot() -> Value {
    json!({"taskCounts":{"running":2,"waitingApproval":0,"interrupted":0,"completed":0,
        "cancelled":0,"failed":0},"approvals":{"snapshot":{"throughSequence":12,"capturedAtUnixMs":1},
        "page":{"items":[],"nextAfter":0,"hasMore":false}}})
}

fn task(id: &str, session: &str) -> Value {
    json!({"id":id,"sessionId":session,"status":"running","backend":{"type":"model"},
        "inputPreview":"","pendingApprovalCount":0,"modelOutcomeUnknown":false,"agentOutcomeUnknown":false})
}

#[test]
fn old_server_omits_preview_in_both_directions() {
    let mut client = connect(false, [snapshot()]);
    let result = client.subscribe_work(&request(), |_| {}).unwrap();
    assert!(result.snapshot.recent_tasks.is_none());
    assert_eq!(result.events.cursor(), 12);
    let (_, sent) = client.into_parts();
    let wire: Value = serde_json::from_slice(sent.split(|b| *b == b'\n').nth(1).unwrap()).unwrap();
    assert!(wire["params"].get("recentTaskLimit").is_none());
}

#[test]
fn negotiated_preview_rejects_missing_duplicate_foreign_or_wrong_size_rows() {
    let mut replies = vec![snapshot()];
    for tasks in [
        json!([task("t1", "s1")]),
        json!([task("t2", "s1"), task("t1", "s1"), task("t0", "s1")]),
        json!([task("t1", "s1"), task("t1", "s1")]),
        json!([task("t2", "s1"), task("t1", "other")]),
        json!([task("t2", "s1"), task("t1", "s1")]),
    ] {
        let mut reply = snapshot();
        reply["recentTasks"] = tasks;
        replies.push(reply);
    }
    let mut client = connect(true, replies);
    for _ in 0..5 {
        assert!(matches!(
            client.work_snapshot(&request(), |_| {}),
            Err(Error::InvalidWorkPreview)
        ));
    }
    let (snapshot, _) = client.work_snapshot(&request(), |_| {}).unwrap();
    assert_eq!(snapshot.recent_tasks.unwrap()[0].id, "t2");
}
