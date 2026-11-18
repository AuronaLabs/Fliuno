use super::*;
use base64::{Engine as _, engine::general_purpose::STANDARD};

#[test]
fn artifact_byte_reads_preserve_utf8_and_nuls_across_restart() {
    let root = std::env::temp_dir()
        .canonicalize()
        .unwrap()
        .join(format!("fliuno-artifact-{}", uuid::Uuid::new_v4()));
    fs::create_dir(&root).unwrap();
    let db = root.join("kernel.db");
    let mut peer = Peer::start(&db);
    let workspace = peer.call("workspace/open", json!({"root":root}));
    let session = peer.call("session/create", json!({"workspaceId":workspace["id"]}));
    let content = "中🌿\0\n".repeat(12_000);
    let started = peer.call(
        "task/start",
        json!({"sessionId":session["id"],"input":content}),
    );
    let task_id = started["id"].as_str().unwrap();
    let waiting = peer.wait_for_status(task_id, "waiting_approval");
    peer.call(
        "approval/respond",
        json!({
            "taskId":task_id,"approvalId":waiting["pendingApproval"]["id"],"decision":"allow_once"
        }),
    );
    peer.wait_for_status(task_id, "completed");
    let summaries = peer.call("artifacts/listSummaries", json!({"taskId":task_id}));
    let artifact = &summaries["items"][0];
    assert_eq!(artifact["contentBytes"], content.len());
    let params = |after: u64, limit: u32| json!({"taskId":task_id,"artifactId":artifact["id"],"after":after,"limit":limit});

    let first = peer.call("artifacts/read", params(0, 4));
    assert_eq!(first["offset"], 0);
    assert_eq!(first["nextOffset"], 4);
    let mut bytes = STANDARD
        .decode(first["dataBase64"].as_str().unwrap())
        .unwrap();
    assert_eq!(bytes, content.as_bytes()[..4]);
    assert!(std::str::from_utf8(&bytes).is_err());
    drop(peer);

    let mut peer = Peer::start(&db);
    let max_page = peer.call("artifacts/read", params(4, 65_536));
    assert_eq!(max_page["nextOffset"], 65_540);
    let default_page = peer.call(
        "artifacts/read",
        json!({
            "taskId":task_id,"artifactId":artifact["id"],"after":max_page["nextOffset"]
        }),
    );
    assert_eq!(default_page["nextOffset"], 65_540 + 32_768);
    let last = peer.call(
        "artifacts/read",
        params(default_page["nextOffset"].as_u64().unwrap(), 65_536),
    );
    for page in [max_page, default_page, last] {
        assert_eq!(page["offset"], bytes.len());
        assert_eq!(page["contentBytes"], content.len());
        bytes.extend(
            STANDARD
                .decode(page["dataBase64"].as_str().unwrap())
                .unwrap(),
        );
        assert_eq!(page["nextOffset"], bytes.len());
    }
    assert_eq!(bytes, content.as_bytes());
    let end = bytes.len() as u64;
    assert_eq!(
        peer.call("artifacts/read", params(end, 64)),
        json!({
            "offset":end,"nextOffset":end,"contentBytes":end,"dataBase64":""
        })
    );
    for invalid in [
        params(end + 1, 64),
        params(u64::MAX, 64),
        params(0, 0),
        params(0, 65_537),
    ] {
        assert_eq!(
            peer.request("artifacts/read", invalid)["error"]["code"],
            -32602
        );
    }
    let other = peer.call(
        "task/start",
        json!({"sessionId":session["id"],"input":"Other task"}),
    );
    assert_eq!(
        peer.request(
            "artifacts/read",
            json!({
                "taskId":other["id"],"artifactId":artifact["id"]
            })
        )["error"]["code"],
        -32004
    );
    drop(peer);
    fs::remove_dir_all(root).unwrap();
}
