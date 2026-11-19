use super::*;

#[test]
fn workspace_titles_survive_migration_and_keep_identity_and_page_positions() {
    let root = std::env::temp_dir()
        .canonicalize()
        .unwrap()
        .join(format!("fliuno-workspace-profile-{}", uuid::Uuid::new_v4()));
    let a = root.join("a").join("project");
    let b = root.join("b").join("project");
    fs::create_dir_all(&a).unwrap();
    fs::create_dir_all(&b).unwrap();
    let db = root.join("kernel.db");
    let mut peer = Peer::start(&db);
    let first = peer.call("workspace/open", json!({"root":a}));
    let second = peer.call("workspace/open", json!({"root":b}));
    let legacy = peer.call("workspace/list", json!({"limit":1}));
    drop(peer);
    rusqlite::Connection::open(&db)
        .unwrap()
        .execute_batch("DROP TABLE workspace_titles; PRAGMA user_version=34;")
        .unwrap();

    let mut peer = Peer::start(&db);
    let first_key = json!({"workspaceId":first["id"]});
    let second_key = json!({"workspaceId":second["id"]});
    assert_eq!(
        peer.call("workspace/getProfile", first_key.clone()),
        json!({"workspace":first})
    );
    assert_eq!(
        peer.request("workspace/setTitle", first_key.clone())["error"]["code"],
        -32602
    );
    for title in ["  ".to_string(), "line\nbreak".into(), "中".repeat(121)] {
        let params = json!({"workspaceId":first["id"],"title":title});
        assert_eq!(
            peer.request("workspace/setTitle", params)["error"]["code"],
            -32602
        );
    }
    assert_eq!(
        peer.request(
            "workspace/setTitle",
            json!({"workspaceId":"missing","title":"Hello"})
        )["error"]["code"],
        -32004
    );
    let rename = json!({"workspaceId":first["id"],"title":"  北区 🌿  "});
    let expected = json!({"workspace":first,"title":"北区 🌿"});
    assert_eq!(peer.call("workspace/setTitle", rename.clone()), expected);
    assert_eq!(peer.call("workspace/setTitle", rename), expected);
    assert_eq!(peer.call("workspace/open", json!({"root":a})), first);
    assert_eq!(peer.call("workspace/list", json!({"limit":1})), legacy);
    let page = peer.call("workspace/listProfiles", json!({"limit":1}));
    assert_eq!(page["items"], json!([expected]));
    assert_eq!(page["nextAfter"], legacy["nextAfter"]);
    assert_eq!(page["hasMore"], true);
    drop(peer);
    // Reading saved profiles must not require the original directory to be present.
    fs::remove_dir(&a).unwrap();
    let mut peer = Peer::start(&db);
    let page = peer.call(
        "workspace/listProfiles",
        json!({"after":page["nextAfter"],"limit":1}),
    );
    assert_eq!(page["items"], json!([{"workspace":second}]));
    assert_eq!(page["hasMore"], false);
    assert_eq!(
        peer.call("workspace/getProfile", first_key.clone()),
        expected
    );
    assert_eq!(
        peer.call("workspace/getProfile", second_key),
        json!({"workspace":second})
    );
    assert_eq!(
        peer.call(
            "workspace/setTitle",
            json!({"workspaceId":first["id"],"title":""})
        ),
        json!({"workspace":first})
    );
    drop(peer);
    let mut peer = Peer::start(&db);
    assert_eq!(
        peer.call("workspace/getProfile", first_key),
        json!({"workspace":first})
    );
    drop(peer);
    fs::remove_dir_all(root).unwrap();
}
