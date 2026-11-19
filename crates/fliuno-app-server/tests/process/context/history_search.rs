use super::*;
use fliuno_client::{Client, HistorySearch, HistorySearchRequest};
use fliuno_protocol::{
    SessionHistoryEntry, SessionHistoryGetParams, SessionHistoryRole, SessionHistorySearchParams,
    WorkspaceHistorySearchParams,
};
use std::{path::Path, process::ChildStdout};

struct Server(Child);

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn connect(db: &Path) -> (Server, Client<BufReader<ChildStdout>, ChildStdin>) {
    let mut server = Server(
        Command::new(env!("CARGO_BIN_EXE_fliuno-app-server"))
            .arg("--db")
            .arg(db)
            .env("FLIUNO_PROVIDER", "demo")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let reader = BufReader::new(server.0.stdout.take().unwrap());
    let writer = server.0.stdin.take().unwrap();
    let client = Client::connect(reader, writer, "history-search-test", |_| {}).unwrap();
    (server, client)
}

#[test]
fn restores_both_search_scopes_and_opens_a_hit_after_restart() {
    let root = std::env::temp_dir().join(format!("fliuno-history-search-{}", uuid::Uuid::new_v4()));
    fs::create_dir(&root).unwrap();
    let db = root.join("kernel.db");
    let mut peer = Peer::start(&db);
    let workspace = peer.call("workspace/open", json!({"root":root}));
    let first = peer.call("session/create", json!({"workspaceId":workspace["id"]}));
    let second = peer.call("session/create", json!({"workspaceId":workspace["id"]}));
    let oldest = approve_report(&mut peer, &first, "History needle one", "oldest");
    let middle = approve_report(&mut peer, &second, "History needle two", "middle");
    let newest = approve_report(&mut peer, &first, "History needle three", "newest");
    drop(peer);

    let session_id = first["id"].as_str().unwrap();
    let session_request = HistorySearchRequest::Session(SessionHistorySearchParams {
        session_id: session_id.to_owned(),
        query: "needle".into(),
        role: Some(SessionHistoryRole::User),
        through_sequence: None,
        before_sequence: None,
        limit: Some(1),
        scan_limit: None,
    });
    let workspace_request = HistorySearchRequest::Workspace(WorkspaceHistorySearchParams {
        workspace_id: workspace["id"].as_str().unwrap().to_owned(),
        query: "needle".into(),
        role: Some(SessionHistoryRole::User),
        through_sequence: None,
        before_sequence: None,
        limit: Some(1),
        scan_limit: None,
    });
    let (through, session_checkpoint, workspace_checkpoint, sparse_checkpoint) = {
        let (_server, mut client) = connect(&db);
        assert_eq!(client.info().capabilities["sessionHistoryGet"], true);
        assert_eq!(client.info().capabilities["maxHistorySearchScanLimit"], 512);
        let fetch = |client: &mut _, request: HistorySearchRequest, scope| {
            let mut search = HistorySearch::new(request);
            let page = search.next_page(client, |_| {}).unwrap().unwrap();
            assert_eq!(page.matches.len(), 1);
            assert_eq!(page.matches[0].task_id, newest["id"]);
            assert_eq!(page.matches[0].session_id.as_deref(), scope);
            let checkpoint = serde_json::to_vec(search.next_request().unwrap()).unwrap();
            (page.through_sequence, checkpoint)
        };
        let (session_through, session_checkpoint) = fetch(&mut client, session_request, None);
        let (workspace_through, workspace_checkpoint) =
            fetch(&mut client, workspace_request.clone(), Some(session_id));
        assert_eq!(session_through, workspace_through);
        let HistorySearchRequest::Workspace(mut sparse) = workspace_request.clone() else {
            unreachable!()
        };
        sparse.query = "needle one".into();
        sparse.limit = None;
        sparse.scan_limit = Some(2);
        let mut search = HistorySearch::new(HistorySearchRequest::Workspace(sparse.clone()));
        let page = search.next_page(&mut client, |_| {}).unwrap().unwrap();
        assert!(page.matches.is_empty());
        assert_eq!(page.scanned_turns, 2);
        assert_eq!(page.through_sequence, session_through);
        let sparse_checkpoint = serde_json::to_vec(search.next_request().unwrap()).unwrap();
        sparse.scan_limit = Some(512);
        let wide = HistorySearch::new(HistorySearchRequest::Workspace(sparse))
            .next_page(&mut client, |_| {})
            .unwrap()
            .unwrap();
        assert_eq!(wide.scanned_turns, 3);
        assert_eq!(wide.matches[0].task_id, oldest["id"]);
        assert_eq!(wide.next_before_sequence, None);
        (
            session_through,
            session_checkpoint,
            workspace_checkpoint,
            sparse_checkpoint,
        )
    };

    let mut peer = Peer::start(&db);
    let later = approve_report(&mut peer, &first, "History needle one later", "later");
    drop(peer);
    let (_server, mut client) = connect(&db);
    let mut session = HistorySearch::new(serde_json::from_slice(&session_checkpoint).unwrap());
    let page = session.next_page(&mut client, |_| {}).unwrap().unwrap();
    assert_eq!(page.through_sequence, through);
    assert_eq!(page.matches.len(), 1);
    assert_eq!(page.matches[0].task_id, oldest["id"]);
    assert!(session.next_page(&mut client, |_| {}).unwrap().is_none());

    let mut workspace = HistorySearch::new(serde_json::from_slice(&workspace_checkpoint).unwrap());
    let page = workspace.next_page(&mut client, |_| {}).unwrap().unwrap();
    assert_eq!(page.through_sequence, through);
    assert_eq!(page.matches.len(), 1);
    let hit = &page.matches[0];
    assert_eq!(hit.task_id, middle["id"]);
    assert_eq!(hit.session_id.as_deref(), second["id"].as_str());
    let read = client
        .get_history_turn(
            &SessionHistoryGetParams {
                session_id: hit.session_id.clone().unwrap(),
                task_id: hit.task_id.clone(),
                through_sequence: Some(page.through_sequence),
                max_bytes: None,
            },
            |_| {},
        )
        .unwrap();
    let SessionHistoryEntry::Turn { turn } = read else {
        panic!("small search result must fit the default lookup budget");
    };
    assert_eq!(turn.completion_sequence, hit.completion_sequence);
    assert_eq!(turn.artifacts.len(), 1);
    let page = workspace.next_page(&mut client, |_| {}).unwrap().unwrap();
    assert_eq!(page.through_sequence, through);
    assert_eq!(page.matches.len(), 1);
    assert_eq!(page.matches[0].task_id, oldest["id"]);
    assert!(workspace.next_page(&mut client, |_| {}).unwrap().is_none());

    let mut sparse = HistorySearch::new(serde_json::from_slice(&sparse_checkpoint).unwrap());
    let page = sparse.next_page(&mut client, |_| {}).unwrap().unwrap();
    assert_eq!(page.through_sequence, through);
    assert_eq!(page.scanned_turns, 1);
    assert_eq!(page.matches[0].task_id, oldest["id"]);
    assert_eq!(page.matches[0].session_id.as_deref(), Some(session_id));
    assert!(sparse.next_page(&mut client, |_| {}).unwrap().is_none());

    let current = HistorySearch::new(workspace_request)
        .next_page(&mut client, |_| {})
        .unwrap()
        .unwrap();
    assert!(current.through_sequence > through);
    assert_eq!(current.matches[0].task_id, later["id"]);
    drop(client);
    drop(_server);
    fs::remove_dir_all(root).unwrap();
}
