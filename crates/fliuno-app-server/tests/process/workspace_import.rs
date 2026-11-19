use super::*;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use http_wire::{Fixture, Response};

const SHA256: &str = "2312394bd99545d9de131c24efb781e765ac1aec243f2ed9347597a793a415e9";

#[test]
fn approved_workspace_import_persists_bytes_without_returning_them_to_the_model() {
    let root = std::env::temp_dir()
        .canonicalize()
        .unwrap()
        .join(format!("fliuno-file-import-{}", uuid::Uuid::new_v4()));
    fs::create_dir(&root).unwrap();
    let path = root.join("source.bin");
    let body: Vec<u8> = (0..=255).cycle().take(256 * 1024).collect();
    fs::write(&path, &body).unwrap();
    let args = json!({"path":"source.bin","title":"Imported file"}).to_string();
    let fixture = Fixture::start(move |request| {
        let input = request.body["input"].as_array().unwrap();
        let output = if let Some(result) = input
            .iter()
            .find(|item| item["type"] == "function_call_output")
        {
            let receipt: Value = serde_json::from_str(result["output"].as_str().unwrap()).unwrap();
            assert_eq!(receipt["contentBytes"], 256 * 1024);
            assert_eq!(receipt["mediaType"], "application/octet-stream");
            assert_eq!(receipt.as_object().unwrap().len(), 4);
            assert_eq!(receipt["sha256"], SHA256);
            assert!(
                receipt["artifactId"]
                    .as_str()
                    .unwrap()
                    .starts_with("artifact_")
            );
            json!([{"type":"message","content":[{"type":"output_text","text":"imported"}]}])
        } else {
            json!([{"type":"function_call","call_id":"import","name":"workspace_import_artifact","arguments":args}])
        };
        Response::json(200, json!({"status":"completed","output":output}))
    });
    let db = root.join("kernel.db");
    let mut peer = Peer::start_with_model_fixture(&db, Some(&fixture.url("/v1/")));
    let workspace = peer.call("workspace/open", json!({"root":root}));
    for (decision, outcome, file_len) in [
        ("deny", "failed", body.len()),
        ("allow_once", "failed", body.len() + 1),
        ("allow_once", "completed", body.len()),
    ] {
        let session = peer.call("session/create", json!({"workspaceId":workspace["id"]}));
        let task = peer.call(
            "task/start",
            json!({"sessionId":session["id"],"input":"Import an existing file"}),
        );
        let id = task["id"].as_str().unwrap();
        let pending = peer.wait_for_status(id, "waiting_approval");
        assert_eq!(
            pending["pendingApproval"]["call"]["name"],
            "workspace_import_artifact"
        );
        assert_eq!(
            peer.call("artifacts/listSummaries", json!({"taskId":id}))["items"],
            json!([])
        );
        // Change the file only after the approval request to verify that the
        // size bound applies to the actual read and not a prior inspection.
        fs::write(&path, &body).unwrap();
        fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap()
            .set_len(file_len as u64)
            .unwrap();
        peer.call(
            "approval/respond",
            json!({"taskId":id,"approvalId":pending["pendingApproval"]["id"],"decision":decision}),
        );
        peer.wait_for_status(id, outcome);
        if outcome == "failed" {
            assert_eq!(
                peer.call("artifacts/listSummaries", json!({"taskId":id}))["items"],
                json!([])
            );
        }
    }
    drop(peer);
    fs::remove_file(&path).unwrap();
    let mut peer = Peer::start(&db);
    let snapshot = peer.call(
        "artifacts/listSnapshot",
        json!({"scope":{"kind":"workspace","workspaceId":workspace["id"]}}),
    );
    assert_eq!(snapshot["items"].as_array().unwrap().len(), 1);
    let summary = &snapshot["items"][0];
    assert_eq!(summary["title"], "Imported file");
    assert_eq!(summary["binary"], true);
    let digest = peer.call(
        "artifacts/digest",
        json!({"taskId":summary["taskId"],"artifactId":summary["id"]}),
    );
    assert_eq!(digest, json!({"contentBytes":body.len(),"sha256":SHA256}));
    let mut restored = Vec::new();
    while restored.len() < body.len() {
        let page = peer.call("artifacts/read", json!({"taskId":summary["taskId"],"artifactId":summary["id"],"after":restored.len(),"limit":65536}));
        restored.extend(
            STANDARD
                .decode(page["dataBase64"].as_str().unwrap())
                .unwrap(),
        );
        assert_eq!(
            restored.len(),
            page["nextOffset"].as_u64().unwrap() as usize
        );
    }
    assert_eq!(restored, body);
    drop(peer);
    fs::remove_dir_all(root).unwrap();
}
