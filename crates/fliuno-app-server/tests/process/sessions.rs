use super::*;
use fliuno_protocol::MAX_SESSION_TITLE_CHARS;

#[test]
fn titles_survive_upgrade_and_restart_without_changing_session_pagination() {
    let temp = std::env::temp_dir().canonicalize().unwrap();
    let root = temp.join(format!("fliuno-sessions-{}", uuid::Uuid::new_v4()));
    fs::create_dir(&root).unwrap();
    let db = root.join("kernel.db");
    let mut peer = Peer::start(&db);
    let workspace = peer.call("workspace/open", json!({"root":root}));
    let workspace_id = &workspace["id"];
    let legacy = peer.call("session/create", json!({"workspaceId":workspace_id}));
    assert!(legacy.get("title").is_none());
    let legacy_id = &legacy["id"];
    drop(peer);
    {
        let old = rusqlite::Connection::open(&db).unwrap();
        old.pragma_update(None, "user_version", 14).unwrap();
    }

    let mut peer = Peer::start(&db);
    assert_eq!(
        peer.call("session/get", json!({"sessionId":legacy_id})),
        legacy
    );
    for title in [
        json!(" "),
        json!("two\nlines"),
        json!("中".repeat(MAX_SESSION_TITLE_CHARS + 1)),
    ] {
        let params = json!({"workspaceId":workspace_id,"title":title});
        assert_eq!(
            peer.request("session/create", params)["error"]["code"],
            -32602
        );
        let params = json!({"sessionId":legacy_id,"title":title});
        assert_eq!(
            peer.request("session/rename", params)["error"]["code"],
            -32602
        );
    }
    assert_eq!(
        peer.call("session/get", json!({"sessionId":legacy_id})),
        legacy
    );
    assert_eq!(
        peer.request("session/rename", json!({"sessionId":legacy_id}))["error"]["code"],
        -32602
    );
    assert_eq!(
        peer.request("session/get", json!({"sessionId":"absent"}))["error"]["code"],
        -32004
    );

    let limit_title = "语".repeat(MAX_SESSION_TITLE_CHARS);
    let named = peer.call(
        "session/create",
        json!({"workspaceId":workspace_id,"title":limit_title}),
    );
    assert_eq!(named["title"], limit_title);
    let page = peer.call(
        "session/list",
        json!({"workspaceId":workspace_id,"limit":1}),
    );
    assert_eq!(page["items"], json!([legacy]));
    assert_eq!(page["hasMore"], true);
    let renamed = peer.call(
        "session/rename",
        json!({"sessionId":legacy_id,"title":"  周报 🗂️  "}),
    );
    assert_eq!(
        renamed,
        json!({"id":legacy_id,"workspaceId":workspace_id,"title":"周报 🗂️"})
    );
    let after = page["nextAfter"].clone();
    let page = peer.call(
        "session/list",
        json!({"workspaceId":workspace_id,"limit":1,"after":after}),
    );
    assert_eq!(page["items"], json!([named]));
    assert_eq!(page["hasMore"], false);
    drop(peer);

    let mut peer = Peer::start(&db);
    assert_eq!(
        peer.call("session/get", json!({"sessionId":legacy_id})),
        renamed
    );
    let page = peer.call(
        "session/list",
        json!({"workspaceId":workspace_id,"limit":1}),
    );
    assert_eq!(page["items"], json!([renamed]));
    assert_eq!(page["nextAfter"], after);
    drop(peer);
    let root = root.canonicalize().unwrap();
    assert_eq!(root.parent(), Some(temp.as_path()));
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn create_request_is_atomic_and_retries_after_a_lost_connection() {
    let root = std::env::temp_dir().join(format!("fliuno-session-key-{}", uuid::Uuid::new_v4()));
    let other_root = root.join("other");
    fs::create_dir(&root).unwrap();
    fs::create_dir(&other_root).unwrap();
    let db = root.join("kernel.db");
    let mut peer = Peer::start(&db);
    let workspace = peer.call("workspace/open", json!({"root":root}));
    let other = peer.call("workspace/open", json!({"root":other_root}));
    let workspace = &workspace["id"];
    let other = &other["id"];
    drop(peer);
    {
        let old = rusqlite::Connection::open(&db).unwrap();
        old.execute_batch("DROP TABLE session_requests; PRAGMA user_version=16;")
            .unwrap();
    }
    let mut peer = Peer::start(&db);
    {
        let db = rusqlite::Connection::open(&db).unwrap();
        db.execute_batch(
            "CREATE TRIGGER reject_session_request BEFORE INSERT ON session_requests
            BEGIN SELECT RAISE(ABORT,'fixture'); END;",
        )
        .unwrap();
    }
    let request =
        json!({"workspaceId":workspace,"clientRequestId":"browser-1","title":" Original "});
    assert_eq!(
        peer.request("session/create", request.clone())["error"]["code"],
        -32603
    );
    assert_eq!(
        peer.call("session/list", json!({"workspaceId":workspace}))["items"],
        json!([])
    );
    {
        let db = rusqlite::Connection::open(&db).unwrap();
        db.execute_batch("DROP TRIGGER reject_session_request")
            .unwrap();
    }

    // A notification commits without giving the client the new session ID.
    writeln!(
        peer.input,
        "{}",
        json!({"jsonrpc":"2.0","method":"session/create","params":request})
    )
    .unwrap();
    peer.input.flush().unwrap();
    let scoped = peer.call("session/list", json!({"workspaceId":workspace}));
    let saved = &scoped["items"][0];
    let session_id = &saved["id"];
    assert_eq!(saved["title"], "Original");
    peer.call(
        "session/rename",
        json!({"sessionId":session_id,"title":"Renamed"}),
    );
    drop(peer);

    let mut peer = Peer::start(&db);
    let mut retry = request.clone();
    retry["title"] = json!("Original");
    let resumed = peer.call("session/create", retry.clone());
    assert_eq!(resumed["id"], *session_id);
    assert_eq!(resumed["title"], "Renamed");
    retry["title"] = json!("Renamed");
    assert_eq!(
        peer.request("session/create", retry)["error"]["code"],
        -32009
    );
    assert_eq!(
        peer.request(
            "session/create",
            json!({"workspaceId":workspace,"clientRequestId":"bad key"})
        )["error"]["code"],
        -32602
    );
    assert_eq!(
        peer.call("session/list", json!({"workspaceId":workspace}))["items"],
        json!([resumed])
    );

    let mut in_other = request;
    in_other["workspaceId"] = other.clone();
    let new_session = peer.call("session/create", in_other);
    assert_ne!(new_session["id"], *session_id);
    assert_eq!(new_session["workspaceId"], *other);
    drop(peer);
    fs::remove_dir(other_root).unwrap();
    for entry in fs::read_dir(&root).unwrap() {
        fs::remove_file(entry.unwrap().path()).unwrap();
    }
    fs::remove_dir(root).unwrap();
}
