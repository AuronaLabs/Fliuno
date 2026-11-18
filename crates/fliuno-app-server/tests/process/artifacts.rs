use super::*;
use base64::{Engine as _, engine::general_purpose::STANDARD};

#[test]
fn artifact_byte_reads_and_catalog_cursors_survive_v20_migration() {
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
    let full = peer.call(
        "artifacts/get",
        json!({"taskId":task_id,"artifactId":artifact["id"]}),
    );
    assert_eq!(full["content"], content);
    drop(peer);
    {
        let legacy = rusqlite::Connection::open(&db).unwrap();
        legacy
            .execute(
                "UPDATE artifacts SET data=?2 WHERE id=?1",
                [artifact["id"].as_str().unwrap(), &full.to_string()],
            )
            .unwrap();
        let empty = json!({"id":"empty-fixture","taskId":task_id,"title":"empty","mediaType":"text/plain","content":""});
        legacy
            .execute(
                "INSERT INTO artifacts(id,task_id,data) VALUES('empty-fixture',?1,?2)",
                [task_id, &empty.to_string()],
            )
            .unwrap();
        legacy
            .execute_batch("DROP TABLE artifact_bodies; PRAGMA user_version=20;")
            .unwrap();
    }

    let mut peer = Peer::start(&db);
    let old_page = peer.call(
        "artifacts/listSummaries",
        json!({"taskId":task_id,"limit":1}),
    );
    assert_eq!(old_page["items"], summaries["items"]);
    assert_eq!(old_page["nextAfter"], summaries["nextAfter"]);
    assert_eq!(old_page["hasMore"], true);
    assert_eq!(
        peer.call(
            "artifacts/getSummary",
            json!({"taskId":task_id,"artifactId":artifact["id"]})
        ),
        *artifact
    );
    let next = peer.call(
        "artifacts/listSummaries",
        json!({"taskId":task_id,"after":summaries["nextAfter"]}),
    );
    assert_eq!(next["items"][0]["id"], "empty-fixture");
    assert_eq!(next["items"][0]["contentBytes"], 0);
    assert_eq!(
        peer.call(
            "artifacts/read",
            json!({"taskId":task_id,"artifactId":"empty-fixture"})
        ),
        json!({
            "offset":0,"nextOffset":0,"contentBytes":0,"dataBase64":""
        })
    );
    let restored = peer.call("artifacts/list", json!({"taskId":task_id}));
    assert_eq!(restored[0], full);
    assert_eq!(restored[1]["content"], "");
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
    let foreign = json!({"taskId":other["id"],"artifactId":artifact["id"]});
    for method in ["artifacts/read", "artifacts/getSummary"] {
        assert_eq!(
            peer.request(method, foreign.clone())["error"]["code"],
            -32004
        );
    }
    drop(peer);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn scoped_artifact_catalog_backfills_and_paginates_across_tasks() {
    let temp = std::env::temp_dir().canonicalize().unwrap();
    let root = temp.join(format!("fliuno-catalog-{}", uuid::Uuid::new_v4()));
    let foreign_root = root.join("foreign");
    fs::create_dir(&root).unwrap();
    fs::create_dir(&foreign_root).unwrap();
    let db = root.join("kernel.db");
    let mut peer = Peer::start(&db);
    let workspace = peer.call("workspace/open", json!({"root":root}));
    let foreign_workspace = peer.call("workspace/open", json!({"root":foreign_root}));
    let first_session = peer.call("session/create", json!({"workspaceId":workspace["id"]}));
    let second_session = peer.call("session/create", json!({"workspaceId":workspace["id"]}));
    let foreign_session = peer.call(
        "session/create",
        json!({"workspaceId":foreign_workspace["id"]}),
    );
    let first = create_artifact(&mut peer, &first_session["id"], "First");
    let foreign = create_artifact(&mut peer, &foreign_session["id"], "Foreign");
    let second = create_artifact(&mut peer, &second_session["id"], "Second");
    peer.call(
        "session/setArchived",
        json!({"sessionId":second_session["id"],"archived":true}),
    );
    drop(peer);

    // Remove the new projection so this is a real backfill of existing rows.
    rusqlite::Connection::open(&db).unwrap().execute_batch(
        "DROP TRIGGER artifact_catalog_insert; DROP TABLE artifact_catalog; PRAGMA user_version=25;",
    ).unwrap();
    let mut peer = Peer::start(&db);
    let catalog = "artifacts/listScopedSummaries";
    let scope = json!({"kind":"workspace","workspaceId":workspace["id"]});
    let head = peer.call(catalog, json!({"scope":scope,"limit":1}));
    assert_eq!(head["items"], json!([first]));
    assert_eq!(head["hasMore"], true);
    let tail = peer.call(
        catalog,
        json!({"scope":scope,"after":head["nextAfter"],"limit":1}),
    );
    assert_eq!(tail["items"], json!([second]));
    assert_eq!(tail["hasMore"], false);
    assert_eq!(
        peer.call(
            catalog,
            json!({"scope":{"kind":"workspace","workspaceId":foreign_workspace["id"]}})
        )["items"],
        json!([foreign])
    );

    let third = create_artifact(&mut peer, &first_session["id"], "Third");
    assert_eq!(
        peer.call(
            catalog,
            json!({"scope":{"kind":"session","sessionId":first_session["id"]}})
        )["items"],
        json!([first, third])
    );
    drop(peer);
    let mut peer = Peer::start(&db);
    let resumed = peer.call(catalog, json!({"scope":scope,"after":tail["nextAfter"]}));
    assert_eq!(resumed["items"], json!([third]));
    assert_eq!(resumed["hasMore"], false);
    assert_eq!(
        peer.call(catalog, json!({"scope":scope,"after":resumed["nextAfter"]})),
        json!({"items":[],"nextAfter":resumed["nextAfter"],"hasMore":false})
    );
    assert_eq!(
        peer.request(
            catalog,
            json!({"scope":{"kind":"session","sessionId":"absent"}})
        )["error"]["code"],
        -32004
    );
    assert_eq!(
        peer.request(catalog, json!({"scope":scope,"after":u64::MAX}))["error"]["code"],
        -32602
    );
    assert_eq!(
        peer.request(catalog, json!({"scope":scope,"limit":101}))["error"]["code"],
        -32602
    );
    drop(peer);
    let root = root.canonicalize().unwrap();
    assert_eq!(root.parent(), Some(temp.as_path()));
    fs::remove_dir_all(root).unwrap();
}

fn create_artifact(peer: &mut Peer, session_id: &Value, input: &str) -> Value {
    let started = peer.call("task/start", json!({"sessionId":session_id,"input":input}));
    let task_id = started["id"].as_str().unwrap();
    let waiting = peer.wait_for_status(task_id, "waiting_approval");
    peer.call("approval/respond", json!({"taskId":task_id,"approvalId":waiting["pendingApproval"]["id"],"decision":"allow_once"}));
    peer.wait_for_status(task_id, "completed");
    let page = peer.call("artifacts/listSummaries", json!({"taskId":task_id}));
    let items = page["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["taskId"], task_id);
    assert!(items[0].get("content").is_none());
    items[0].clone()
}
