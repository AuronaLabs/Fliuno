use super::*;
use fliuno_client::SessionCatalog;
use fliuno_protocol::{Session, SessionListOrder, SessionListParams};

#[test]
fn newest_session_catalog_resumes_after_restart_without_mixing_new_rows_or_scopes() {
    let root =
        std::env::temp_dir().join(format!("fliuno-session-catalog-{}", uuid::Uuid::new_v4()));
    fs::create_dir_all(root.join("other")).unwrap();
    let db = root.join("kernel.db");
    let mut peer = Peer::start(&db);
    let workspace = peer.call("workspace/open", json!({"root":root}));
    let foreign = peer.call("workspace/open", json!({"root":root.join("other")}));
    let workspace_id = workspace["id"].as_str().unwrap().to_owned();
    let create = |peer: &mut Peer, workspace: &str, title: &str| -> Session {
        serde_json::from_value(peer.call(
            "session/create",
            json!({"workspaceId":workspace,"title":title}),
        ))
        .unwrap()
    };
    let first = create(&mut peer, &workspace_id, "first");
    let second = create(&mut peer, &workspace_id, "second");
    let foreign = create(&mut peer, foreign["id"].as_str().unwrap(), "foreign");
    let third = create(&mut peer, &workspace_id, "third");
    let fourth = create(&mut peer, &workspace_id, "fourth");
    for id in [&fourth.id, &second.id] {
        peer.call(
            "session/setArchived",
            json!({"sessionId":id,"archived":true}),
        );
    }
    let legacy = peer.call(
        "session/list",
        json!({"workspaceId":workspace_id,"limit":1}),
    );
    assert_eq!(legacy["items"][0]["id"], first.id);
    let all = peer.call(
        "session/list",
        json!({"workspaceId":workspace_id,"order":"newest_first"}),
    );
    assert_eq!(
        all["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|item| item["id"].as_str().unwrap())
            .collect::<Vec<_>>(),
        [&fourth.id, &third.id, &second.id, &first.id]
    );
    assert!(
        !all["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["id"] == foreign.id)
    );
    let archived = peer.call(
        "session/list",
        json!({"workspaceId":workspace_id,"archived":true,"order":"newest_first","limit":1}),
    );
    assert_eq!(archived["items"][0]["id"], fourth.id);
    assert_eq!(archived["hasMore"], true);
    let archived_next = peer.call("session/list", json!({"workspaceId":workspace_id,"archived":true,"order":"newest_first","after":archived["nextAfter"],"limit":1}));
    assert_eq!(archived_next["items"][0]["id"], second.id);
    assert_eq!(archived_next["hasMore"], false);
    for bad in [
        json!({"workspaceId":workspace_id,"order":"recent_activity"}),
        json!({"workspaceId":workspace_id,"order":"newest_first","after":u64::MAX}),
    ] {
        assert_eq!(peer.request("session/list", bad)["error"]["code"], -32602);
    }
    drop(peer);

    let connect = || {
        let child = Command::new(env!("CARGO_BIN_EXE_fliuno-app-server"))
            .arg("--db")
            .arg(&db)
            .env("FLIUNO_PROVIDER", "demo")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut server = Server(child);
        let writer = server.0.stdin.take().unwrap();
        let reader = BufReader::new(server.0.stdout.take().unwrap());
        let client = Client::connect(reader, writer, "session-catalog-test", |_| {}).unwrap();
        assert_eq!(client.info().capabilities["sessionListOrdering"], true);
        (server, client)
    };
    let (server, mut client) = connect();
    let request = SessionListParams {
        workspace_id: workspace_id.clone(),
        archived: Some(false),
        after: 0,
        limit: 1,
        order: Some(SessionListOrder::NewestFirst),
        query: None,
    };
    let mut active = SessionCatalog::new(request.clone());
    assert_eq!(
        active
            .next_page(&mut client, |_| {})
            .unwrap()
            .unwrap()
            .items[0]
            .id,
        third.id
    );
    let saved = serde_json::to_vec(active.next_request().unwrap()).unwrap();
    drop(client);
    drop(server);

    let mut peer = Peer::start(&db);
    let newest = create(&mut peer, &workspace_id, "newest");
    let renamed = peer.call(
        "session/rename",
        json!({"sessionId":first.id,"title":"renamed"}),
    );
    drop(peer);
    let (server, mut client) = connect();
    let restored = serde_json::from_slice(&saved).unwrap();
    let mut active = SessionCatalog::new(restored);
    let last = active.next_page(&mut client, |_| {}).unwrap().unwrap();
    assert_eq!(last.items[0].id, first.id);
    assert_eq!(last.items[0].title.as_deref(), renamed["title"].as_str());
    assert_eq!(last.next_after, legacy["nextAfter"].as_u64().unwrap());
    assert!(!last.has_more);
    assert!(active.next_request().is_none());
    assert!(active.next_page(&mut client, |_| {}).unwrap().is_none());
    let refreshed = SessionCatalog::new(request)
        .next_page(&mut client, |_| {})
        .unwrap()
        .unwrap();
    assert_eq!(refreshed.items[0].id, newest.id);
    drop(client);
    drop(server);
    fs::remove_dir_all(root).unwrap();
}
