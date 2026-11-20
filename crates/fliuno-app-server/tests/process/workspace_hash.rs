//! Verify the approval and persisted result of one bounded hash batch.
use super::*;
use http_wire::{Fixture, Response};

#[test]
fn workspace_hash_batch_checks_current_bytes_after_persisted_approval() {
    const LIMIT: u64 = 8 * 1024 * 1024;
    let root = std::env::temp_dir()
        .canonicalize()
        .unwrap()
        .join(format!("fliuno-hash-batch-{}", uuid::Uuid::new_v4()));
    fs::create_dir(&root).unwrap();
    fs::create_dir(root.join("directory")).unwrap();
    fs::write(root.join("changed.bin"), b"initial bytes").unwrap();
    fs::write(root.join("empty"), []).unwrap();
    for (name, size) in [("exact", LIMIT), ("oversized", LIMIT + 1)] {
        fs::File::create(root.join(name))
            .unwrap()
            .set_len(size)
            .unwrap();
    }
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink("changed.bin", root.join("linked")).unwrap();
        assert!(
            Command::new("mkfifo")
                .arg(root.join("fifo"))
                .status()
                .unwrap()
                .success()
        );
    }
    let paths = json!([
        "empty",
        "missing",
        "changed.bin",
        "exact",
        "oversized",
        "directory",
        "linked",
        "fifo"
    ]);
    let args = json!({"paths":paths});
    let expected = json!({"entries":[
        {"path":"empty","status":"hashed","bytes":0,"sha256":"e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"},
        {"path":"missing","status":"unavailable"},
        {"path":"changed.bin","status":"hashed","bytes":3,"sha256":"a90a10503fbfc95789ff38a1bb5039cb71869ab9c0eb1cb51c4a9099f2933c6b"},
        {"path":"exact","status":"hashed","bytes":LIMIT,"sha256":"2daeb1f36095b44b318410b3f4e8b5d989dcc7bb023d1426c492dab0a3053e74"},
        {"path":"oversized","status":"unavailable"},
        {"path":"directory","status":"unavailable"},
        {"path":"linked","status":"unavailable"},
        {"path":"fifo","status":"unavailable"}
    ]});
    let proposed = args.clone();
    let digest_receipt = expected.clone();
    let fixture = Fixture::start(move |request| {
        let input = request.body["input"].as_array().unwrap();
        let output = if let Some(result) = input
            .iter()
            .find(|value| value["type"] == "function_call_output")
        {
            let value: Value = serde_json::from_str(result["output"].as_str().unwrap()).unwrap();
            assert_eq!(value, digest_receipt);
            json!([{"type":"message","content":[{"type":"output_text","text":"checksum report ready"}]}])
        } else {
            let text = input
                .iter()
                .find(|value| value["role"] == "user")
                .unwrap()
                .to_string();
            let arguments = if text.contains("validate invalid paths") {
                json!({"paths":["changed.bin","../ outside"]})
            } else {
                proposed.clone()
            };
            json!([{"type":"function_call","call_id":"hash-batch","name":"workspace_sha256_batch","arguments":arguments.to_string()}])
        };
        Response::json(200, json!({"status":"completed","output":output}))
    });
    let db = root.join("kernel.db");
    let uri = fixture.url("/v1/");
    let mut peer = Peer::start_with_model_fixture(&db, Some(&uri));
    let ws = peer.call("workspace/open", json!({"root":root}));
    let specs = peer.call("tools/list", json!({}));
    let spec = specs
        .as_array()
        .unwrap()
        .iter()
        .find(|spec| spec["name"] == "workspace_sha256_batch")
        .unwrap();
    assert_eq!(spec["effect"], "workspace_read");
    assert_eq!(spec["inputSchema"]["properties"]["paths"]["maxItems"], 8);

    let session = peer.call("session/create", json!({"workspaceId":ws["id"]}));
    let start = |peer: &mut Peer, input: &str| {
        let task = peer.call(
            "task/start",
            json!({"sessionId":session["id"],"input":input,"context":{"mode":"isolated"}}),
        );
        task["id"].as_str().unwrap().to_owned()
    };
    let invalid = start(&mut peer, "validate invalid paths");
    let failed = peer.wait_for_status(&invalid, "failed");
    assert!(failed["pendingApproval"].is_null());

    let denied = start(&mut peer, "prepare a hash batch for denial");
    let pending = peer.wait_for_status(&denied, "waiting_approval");
    assert_eq!(pending["pendingApproval"]["call"]["arguments"], args);
    let decide = |peer: &mut Peer, id: &str, pending: &Value, decision: &str| {
        peer.call(
            "approval/respondDecision",
            json!({"taskId":id,"approvalId":pending["pendingApproval"]["id"],"decision":decision}),
        );
    };
    decide(&mut peer, &denied, &pending, "deny");
    let failed = peer.wait_for_status(&denied, "failed");
    assert!(
        failed["messages"]
            .as_array()
            .unwrap()
            .iter()
            .all(|message| message["role"] != "tool_result")
    );

    let approved = start(&mut peer, "prepare a hash batch to execute");
    let pending = peer.wait_for_status(&approved, "waiting_approval");
    assert_eq!(pending["pendingApproval"]["call"]["arguments"], args);
    drop(peer);
    fs::write(root.join("changed.bin"), [0, 255, 65]).unwrap();
    let mut peer = Peer::start_with_model_fixture(&db, Some(&uri));
    assert_eq!(peer.call("task/get", json!({"taskId":approved})), pending);
    decide(&mut peer, &approved, &pending, "allow_once");
    let done = peer.wait_for_status(&approved, "completed");
    assert!(
        done["messages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|message| message["role"] == "tool_result" && message["output"] == expected)
    );
    assert_eq!(
        peer.call("artifacts/list", json!({"taskId":approved})),
        json!([])
    );
    drop(peer);
    drop(fixture);
    let mut peer = Peer::start(&db);
    assert_eq!(peer.call("task/get", json!({"taskId":approved})), done);
    drop(peer);
    fs::remove_dir_all(root).unwrap();
}
