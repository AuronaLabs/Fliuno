//! End-to-end binary artifact persistence across the model, approval and RPC boundaries.
use super::*;
use http_wire::{Fixture, Response};

#[test]
fn binary_artifact_requires_approval_and_preserves_raw_bytes_across_restart() {
    let root = std::env::temp_dir().join(format!("fliuno-binary-{}", uuid::Uuid::new_v4()));
    fs::create_dir(&root).unwrap();
    let db = root.join("kernel.db");
    let expected: Vec<u8> = (0..=255).cycle().take(150_003).collect();
    let encoded = STANDARD.encode(&expected);
    let arguments = json!({
        "title":"binary result", "mediaType":"application/octet-stream", "contentBase64":encoded
    })
    .to_string();
    let fixture = Fixture::start(move |request| {
        assert_eq!(request.path, "/v1/responses");
        let input = request.body["input"].as_array().unwrap();
        let output = if let Some(result) =
            input.iter().find(|v| v["type"] == "function_call_output")
        {
            let receipt: Value = serde_json::from_str(result["output"].as_str().unwrap()).unwrap();
            assert_eq!(receipt["contentBytes"], 150_003);
            assert!(receipt.get("contentBase64").is_none());
            json!([{"type":"message","content":[{"type":"output_text","text":"created"}]}])
        } else {
            assert!(
                request.body["tools"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|tool| tool["name"] == "artifact_create_binary")
            );
            json!([{"type":"function_call","call_id":"create-binary","name":"artifact_create_binary","arguments":arguments}])
        };
        Response::json(200, json!({"status":"completed","output":output}))
    });
    let mut peer = Peer::start_with_model_fixture(&db, Some(&fixture.url("/v1/")));
    let workspace = peer.call("workspace/open", json!({"root":root}));
    let mut task_ids = Vec::new();
    for (decision, outcome) in [("deny", "failed"), ("allow_once", "completed")] {
        let session = peer.call("session/create", json!({"workspaceId":workspace["id"]}));
        let started = peer.call(
            "task/start",
            json!({"sessionId":session["id"],"input":"create a binary artifact"}),
        );
        let id = started["id"].as_str().unwrap();
        let waiting = peer.wait_for_status(id, "waiting_approval");
        assert_eq!(
            waiting["pendingApproval"]["call"]["name"],
            "artifact_create_binary"
        );
        assert_eq!(
            peer.call("artifacts/listSummaries", json!({"taskId":id}))["items"],
            json!([])
        );
        peer.call(
            "approval/respond",
            json!({
                "taskId":id,"approvalId":waiting["pendingApproval"]["id"],"decision":decision
            }),
        );
        peer.wait_for_status(id, outcome);
        task_ids.push(id.to_owned());
    }
    let denied = &task_ids[0];
    let task_id = &task_ids[1];
    assert_eq!(
        peer.call("artifacts/list", json!({"taskId":denied})),
        json!([])
    );
    let summaries = peer.call("artifacts/listSummaries", json!({"taskId":task_id}));
    let summary = &summaries["items"][0];
    assert_eq!(summary["binary"], true);
    assert_eq!(summary["contentBytes"], expected.len());
    assert_eq!(summary["mediaType"], "application/octet-stream");
    let artifact_id = summary["id"].as_str().unwrap();
    let identity = json!({"taskId":task_id,"artifactId":artifact_id});
    drop(peer);
    let conn = rusqlite::Connection::open(&db).unwrap();
    let stored: Vec<u8> = conn
        .query_row(
            "SELECT body FROM artifact_bodies WHERE artifact_id=?1",
            [artifact_id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(stored, expected);
    drop(conn);

    let mut peer = Peer::start(&db);
    assert_eq!(
        peer.call("artifacts/getSummary", identity.clone()),
        *summary
    );
    let catalog = peer.call(
        "artifacts/listScopedSummaries",
        json!({"scope":{"kind":"workspace","workspaceId":workspace["id"]}}),
    );
    assert_eq!(catalog["items"], json!([summary]));
    let artifact = peer.call("artifacts/get", identity.clone());
    assert!(artifact.get("content").is_none());
    assert_eq!(artifact["contentBase64"], encoded);
    assert_eq!(
        peer.call("artifacts/list", json!({"taskId":task_id})),
        json!([artifact])
    );
    let parsed: fliuno_protocol::Artifact = serde_json::from_value(artifact).unwrap();
    assert_eq!(parsed.body.as_bytes(), expected);
    let mut copied = Vec::new();
    while copied.len() < expected.len() {
        let page = peer.call(
            "artifacts/read",
            json!({
                "taskId":task_id,"artifactId":artifact_id,"after":copied.len(),"limit":65536
            }),
        );
        assert_eq!(page["offset"], copied.len());
        let next = page["nextOffset"].as_u64().unwrap() as usize;
        assert!(next > copied.len());
        copied.extend(
            STANDARD
                .decode(page["dataBase64"].as_str().unwrap())
                .unwrap(),
        );
        assert_eq!(copied.len(), next);
    }
    assert_eq!(copied, expected);
    assert_eq!(
        peer.call("artifacts/digest", identity.clone()),
        json!({
            "contentBytes":150003,"sha256":"41827d9bc4b6cda11c69b2062a8c1b3b0f1f1da37063df5875899bcb329a2649"
        })
    );
    assert_eq!(
        peer.request(
            "artifacts/get",
            json!({"taskId":denied,"artifactId":artifact_id})
        )["error"]["code"],
        -32004
    );
    drop(peer);
    for entry in fs::read_dir(&root).unwrap() {
        fs::remove_file(entry.unwrap().path()).unwrap();
    }
    fs::remove_dir(root).unwrap();
}
