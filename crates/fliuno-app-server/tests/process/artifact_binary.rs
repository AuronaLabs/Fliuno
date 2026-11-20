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

#[test]
fn artifact_task_input_keeps_scope_order_and_retry_identity() {
    const PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+yhE0AAAAASUVORK5CYII=";
    const PDF: &str = "JVBERi0xLjQKMSAwIG9iago8PCAvVHlwZSAvQ2F0YWxvZyAvUGFnZXMgMiAwIFIgPj4KZW5kb2JqCjIgMCBvYmoKPDwgL1R5cGUgL1BhZ2VzIC9LaWRzIFszIDAgUl0gL0NvdW50IDEgPj4KZW5kb2JqCjMgMCBvYmoKPDwgL1R5cGUgL1BhZ2UgL1BhcmVudCAyIDAgUiAvTWVkaWFCb3ggWzAgMCAxMCAxMF0gPj4KZW5kb2JqCnhyZWYKMCA0CjAwMDAwMDAwMDAgNjU1MzUgZiAKMDAwMDAwMDAwOSAwMDAwMCBuIAowMDAwMDAwMDU4IDAwMDAwIG4gCjAwMDAwMDAxMTUgMDAwMDAgbiAKdHJhaWxlcgo8PCAvU2l6ZSA0IC9Sb290IDEgMCBSID4+CnN0YXJ0eHJlZgoxODIKJSVFT0YK";
    let root = std::env::temp_dir().join(format!("fliuno-artifact-input-{}", uuid::Uuid::new_v4()));
    fs::create_dir(&root).unwrap();
    let db = root.join("kernel.db");
    let fixture = Fixture::start(move |request| {
        assert_eq!(request.path, "/v1/responses");
        let input = request.body["input"].as_array().unwrap();
        let current = &input.last().unwrap()["content"];
        let complete =
            || json!([{"type":"message","content":[{"type":"output_text","text":"done"}]}]);
        let output = if let Some(parts) = current.as_array() {
            assert_eq!(
                parts,
                &vec![
                    json!({"type":"input_text","text":"Review the artifacts"}),
                    json!({"type":"input_file","filename":"document.pdf","file_data":format!("data:application/pdf;base64,{PDF}")}),
                    json!({"type":"input_image","detail":"auto","image_url":format!("data:image/png;base64,{PNG}")}),
                ]
            );
            complete()
        } else if input
            .iter()
            .any(|item| item["type"] == "function_call_output")
        {
            complete()
        } else {
            let (media, data) = match current.as_str().unwrap() {
                "create pdf" => ("application/pdf", PDF),
                "create png" => ("image/png", PNG),
                other => panic!("unexpected model request: {other}"),
            };
            let args =
                json!({"title":"attachment","mediaType":media,"contentBase64":data}).to_string();
            json!([{"type":"function_call","call_id":"new-artifact","name":"artifact_create_binary","arguments":args}])
        };
        Response::json(200, json!({"status":"completed","output":output}))
    });
    let mut peer = Peer::start_with_model_fixture(&db, Some(&fixture.url("/v1/")));
    let workspace = peer.call("workspace/open", json!({"root":root}));
    let session = peer.call("session/create", json!({"workspaceId":workspace["id"]}));
    let other = peer.call("session/create", json!({"workspaceId":workspace["id"]}));
    let mut create_artifact =
        |text| {
            let task = peer.call(
                "task/start",
                json!({"sessionId":session["id"],"input":text}),
            );
            let id = task["id"].as_str().unwrap();
            let waiting = peer.wait_for_status(id, "waiting_approval");
            peer.call("approval/respond", json!({
            "taskId":id,"approvalId":waiting["pendingApproval"]["id"],"decision":"allow_once"
        }));
            peer.wait_for_status(id, "completed");
            let manifest = peer.call("artifacts/listSummaries", json!({"taskId":id}));
            json!({"taskId":id,"artifactId":manifest["items"][0]["id"]})
        };
    let document = create_artifact("create pdf");
    let image = create_artifact("create png");
    let request = json!({
        "sessionId":session["id"],"clientRequestId":"reuse-artifacts",
        "input":"Review the artifacts","artifactInputs":[document,image]
    });
    let mut cross_session = request.clone();
    cross_session["sessionId"] = other["id"].clone();
    assert_eq!(
        peer.request("task/startSummary", cross_session)["error"]["code"],
        -32004
    );
    let mut invalid_pair = request.clone();
    invalid_pair["artifactInputs"][0]["taskId"] = image["taskId"].clone();
    assert_eq!(
        peer.request("task/startSummary", invalid_pair)["error"]["code"],
        -32004
    );
    let mut over_budget = request.clone();
    let oversized = STANDARD.encode([b"%PDF-".as_slice(), &vec![b' '; 384 * 1024 - 5]].concat());
    over_budget["input"] = json!([{"type":"pdf","data":oversized}]);
    assert_eq!(
        peer.request("task/startSummary", over_budget)["error"]["code"],
        -32602
    );
    let mut agent = request.clone();
    agent["backend"] = json!({"type":"external_agent","agentId":"external"});
    assert_eq!(
        peer.request("task/startSummary", agent)["error"]["code"],
        -32602
    );
    let summary = peer.call("task/startSummary", request.clone());
    let id = summary["id"].as_str().unwrap();
    assert_eq!(
        summary["inputPreview"],
        "Review the artifacts[document][image]"
    );
    peer.wait_for_status(id, "completed");
    drop(peer);

    // Alter the source body in this isolated fixture to prove retries read
    // the receipt and the original task snapshot without resolving references.
    let connection = rusqlite::Connection::open(&db).unwrap();
    connection
        .execute(
            "UPDATE artifact_bodies SET body=x'00' WHERE artifact_id=?1",
            [image["artifactId"].as_str().unwrap()],
        )
        .unwrap();
    drop(connection);
    let mut peer = Peer::start(&db);
    assert_eq!(peer.call("task/startSummary", request.clone())["id"], id);
    let full = peer.call("task/start", request.clone());
    assert_eq!(full["id"], id);
    assert_eq!(full["messages"][0]["parts"][1]["data"], PDF);
    assert_eq!(full["messages"][0]["parts"][2]["data"], PNG);
    let mut changed = request.clone();
    changed["artifactInputs"] = json!([image, document]);
    assert_eq!(
        peer.request("task/startSummary", changed)["error"]["code"],
        -32009
    );
    let mut inline = request.clone();
    inline.as_object_mut().unwrap().remove("artifactInputs");
    inline["input"] = full["messages"][0]["parts"].clone();
    assert_eq!(peer.request("task/start", inline)["error"]["code"], -32009);
    let mut fresh = request;
    fresh["clientRequestId"] = json!("new-reference");
    assert_eq!(
        peer.request("task/startSummary", fresh)["error"]["code"],
        -32602
    );
    drop(peer);
    fs::remove_dir_all(root).unwrap();
}
