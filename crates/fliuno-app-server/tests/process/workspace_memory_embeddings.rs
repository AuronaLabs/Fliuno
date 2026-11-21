use super::*;
use http_wire::{Fixture, Response};
use std::path::Path;

fn launch(db: &Path, config: &Path) -> Peer {
    let child = Command::new(env!("CARGO_BIN_EXE_fliuno-app-server"))
        .args(["--db", db.to_str().unwrap(), "--memory-embedding-config"])
        .arg(config)
        .env("FLIUNO_PROVIDER", "demo")
        .env("FLIUNO_TEST_EMBEDDING_KEY", "embedding-fixture-key")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    let mut peer = Peer::from_child(child);
    let init = peer.call(
        "initialize",
        json!({"protocolVersion":"0.1","clientName":"embedding-fixture"}),
    );
    assert_eq!(init["capabilities"]["taskMemoryHybridRetrieval"], true);
    assert_eq!(init["capabilities"]["taskMemoryOptOut"], true);
    peer
}

#[test]
fn task_memory_uses_opt_in_hybrid_retrieval_and_freezes_provenance() {
    let root = std::env::temp_dir().join(format!("fliuno-embeddings-{}", uuid::Uuid::new_v4()));
    let first_root = root.join("first");
    let other_root = root.join("other");
    fs::create_dir_all(&first_root).unwrap();
    fs::create_dir(&other_root).unwrap();
    let db = root.join("kernel.db");
    let mut peer = Peer::start(&db);
    let first = peer.call("workspace/open", json!({"root":first_root}));
    let other = peer.call("workspace/open", json!({"root":other_root}));
    let session = peer.call("session/create", json!({"workspaceId":first["id"]}));
    let other_session = peer.call("session/create", json!({"workspaceId":other["id"]}));
    let put = |peer: &mut Peer, workspace: &Value, key: &str, content: &str| {
        peer.call(
            "workspace/memory/put",
            json!({"workspaceId":workspace["id"],"key":key,"content":content}),
        )
    };
    let preference = put(&mut peer, &first, "build-preference", "I prefer Rust.");
    let symbol = put(&mut peer, &first, "R", "Single letter selector.");
    let probe = put(&mut peer, &first, "index-race", "Old text.");
    put(&mut peer, &other, "private", "OTHER_WORKSPACE");
    let prompt = "设计一个高性能操作系统内核";
    let params = json!({"sessionId":session["id"],"input":[
        {"type":"text","text":prompt},
        {"type":"image","mimeType":"image/png","data":"iVBORw0KGgo="}
    ],"clientRequestId":"hybrid-1"});
    let mut baseline = params.clone();
    baseline["clientRequestId"] = json!("baseline");
    assert!(peer.call("task/start", baseline)["context"]["memories"].is_null());
    drop(peer);

    let (index_started, index_received) = mpsc::channel();
    let (release_index, wait_for_release) = mpsc::channel();
    let wait_for_release = std::sync::Mutex::new(wait_for_release);
    let fixture = Fixture::start(move |request| {
        assert_eq!(request.method, "POST");
        assert_eq!(request.path, "/embeddings");
        assert_eq!(
            request.headers["authorization"],
            "Bearer embedding-fixture-key"
        );
        assert_eq!(request.body["dimensions"], 3);
        assert_eq!(request.body["model"], "fixture");
        let inputs = request.body["input"].as_array().unwrap();
        if inputs.iter().any(|item| item == "index-race\nOld text.") {
            index_started.send(()).unwrap();
            wait_for_release
                .lock()
                .unwrap()
                .recv_timeout(Duration::from_secs(2))
                .unwrap();
        }
        if inputs
            .iter()
            .any(|item| item.as_str().unwrap().starts_with("timeout"))
        {
            return Response::hold();
        }
        let data: Vec<_> = inputs
            .iter()
            .enumerate()
            .rev()
            .map(|(index, raw)| {
                let text = raw.as_str().unwrap();
                assert!(!text.contains("OTHER_WORKSPACE") && !text.contains("iVBOR"));
                let embedding = if text.starts_with("build-preference\nI prefer Rust")
                    || text.trim() == prompt
                {
                    [1.0, 0.0, 0.0]
                } else if text.starts_with("R\nSingle") {
                    [0.0, 1.0, 0.0]
                } else if text.starts_with("index-race\n") {
                    [-1.0, 0.0, 0.0]
                } else {
                    [0.1, 0.0, 1.0]
                };
                json!({"index":index,"embedding":embedding})
            })
            .collect();
        Response::json(200, json!({"data":data}))
    });
    let config = root.join("embedding.json");
    fs::write(
        &config,
        json!({"endpoint":fixture.url("/embeddings"),"model":"fixture",
        "modelIdentity":"fixture-v1","dimensions":3,"apiKeyEnv":"FLIUNO_TEST_EMBEDDING_KEY",
        "workspaces":[first["id"]]})
        .to_string(),
    )
    .unwrap();
    let mut peer = launch(&db, &config);
    index_received.recv_timeout(Duration::from_secs(5)).unwrap();
    peer.call(
        "workspace/memory/put",
        json!({"workspaceId":first["id"],"key":"index-race","content":"Revised text.","expectedRevision":1}),
    );
    release_index.send(()).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    let sql = rusqlite::Connection::open(&db).unwrap();
    loop {
        let count: i64 = sql
            .query_row("SELECT count(*) FROM workspace_memory_vectors", [], |r| {
                r.get(0)
            })
            .unwrap();
        if count == 3 {
            break;
        }
        assert!(Instant::now() < deadline, "index failed to populate");
        thread::sleep(Duration::from_millis(20));
    }
    let revision: i64 = sql
        .query_row(
            "SELECT revision FROM workspace_memory_vectors WHERE memory_id=?1",
            [probe["id"].as_str().unwrap()],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(revision, 2);

    let summary = peer.call("task/startSummary", params.clone());
    let id = &summary["id"];
    let frozen = peer.call("task/get", json!({"taskId":id}))["context"].clone();
    assert_eq!(frozen["memories"][0]["id"], preference["id"]);
    assert_eq!(frozen["memories"][0]["matchedBy"], "semantic");
    assert_eq!(frozen["memories"].as_array().unwrap().len(), 1);
    let count = fixture.requests().len();
    assert_eq!(peer.call("task/start", params.clone())["context"], frozen);
    for context in [json!({"disableMemory":true}), json!({"mode":"isolated"})] {
        let task = peer.call(
            "task/start",
            json!({"sessionId":session["id"],"input":"Rust R","context":context}),
        );
        assert!(task["context"]["memories"].is_null());
    }
    let outside = peer.call(
        "task/start",
        json!({"sessionId":other_session["id"],"input":"OTHER_WORKSPACE"}),
    );
    assert!(outside["context"]["memoryFallback"].is_null());
    assert_eq!(fixture.requests().len(), count);

    let keyword = peer.call(
        "task/start",
        json!({"sessionId":session["id"],"input":"Use `R`"}),
    );
    assert_eq!(keyword["context"]["memories"][0]["id"], symbol["id"]);
    assert_eq!(keyword["context"]["memories"][0]["matchedBy"], "keyword");
    assert_eq!(
        keyword["context"]["memoryFallback"],
        "below_similarity_threshold"
    );

    let start = Instant::now();
    let fallback = peer.call(
        "task/start",
        json!({"sessionId":session["id"],"input":"timeout Rust"}),
    );
    assert!(start.elapsed() < Duration::from_secs(2));
    assert_eq!(
        fallback["context"]["memoryFallback"],
        "embedding_unavailable"
    );
    assert_eq!(fallback["context"]["memories"][0]["matchedBy"], "keyword");

    sql.execute(
        "UPDATE workspace_memory_vectors SET vector=zeroblob(length(vector)) WHERE memory_id=?1",
        [preference["id"].as_str().unwrap()],
    )
    .unwrap();
    let fallback = peer.call(
        "task/start",
        json!({"sessionId":session["id"],"input":"Rust"}),
    );
    assert_eq!(fallback["context"]["memoryFallback"], "corrupt_index");
    assert_eq!(fallback["context"]["memories"][0]["id"], preference["id"]);

    peer.call(
        "workspace/memory/put",
        json!({"workspaceId":first["id"],"key":"build-preference",
        "content":"Prefer Go now.","expectedRevision":1}),
    );
    let after = peer.call(
        "task/start",
        json!({"sessionId":session["id"],"input":prompt}),
    );
    assert!(after["context"]["memories"].is_null());
    assert_eq!(
        peer.call("task/get", json!({"taskId":id}))["context"],
        frozen
    );
    drop(sql);
    drop(peer);
    fs::remove_dir_all(root).unwrap();
}
