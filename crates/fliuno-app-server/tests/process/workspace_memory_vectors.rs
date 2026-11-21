use super::*;

#[test]
fn workspace_vectors_rank_reopen_invalidate_and_rebuild() {
    let root = std::env::temp_dir().join(format!("fliuno-memory-vectors-{}", uuid::Uuid::new_v4()));
    let first = root.join("first");
    let second = root.join("second");
    fs::create_dir_all(&first).unwrap();
    fs::create_dir(&second).unwrap();
    let db = root.join("kernel.db");
    let mut peer = Peer::start(&db);
    let first = peer.call("workspace/open", json!({"root":first}));
    let second = peer.call("workspace/open", json!({"root":second}));
    let put = |peer: &mut Peer, workspace: &Value, key, content| {
        peer.call(
            "workspace/memory/put",
            json!({
                "workspaceId":workspace["id"],"key":key,"content":content
            }),
        )
    };
    let primary = put(&mut peer, &first, "compile", "Run Cargo before a release");
    let alternate = put(&mut peer, &first, "review", "Review tests and policy");
    let outside = put(&mut peer, &second, "private", "Private task notes");
    let batch = |workspace: &Value, vectors: Value| json!({"workspaceId":workspace["id"],"modelIdentity":"fixture-v1","vectors":vectors});
    let vector = |record: &Value, a: f32, b: f32| json!({"memoryId":record["id"],"revision":record["revision"],"values":[a,b]});
    let search = |peer: &mut Peer, workspace: &Value, embedding: Value| {
        peer.call(
            "workspace/memory/semanticSearch",
            json!({
                "workspaceId":workspace["id"],"query":"Cargo","limit":8,"embedding":embedding
            }),
        )
    };
    let query = json!({"modelIdentity":"fixture-v1","values":[6,0]});
    let keyword = search(&mut peer, &first, Value::Null);
    assert_eq!(keyword["mode"], "keyword");
    assert_eq!(keyword["fallbackReason"], "embedding_unavailable");
    assert_eq!(keyword["items"][0]["memory"]["id"], primary["id"]);

    let foreign = batch(
        &first,
        json!([vector(&primary, 2.0, 0.0), vector(&outside, 1.0, 0.0)]),
    );
    assert!(peer.request("workspace/memory/indexVectors", foreign)["error"].is_object());
    assert_eq!(
        search(&mut peer, &first, query.clone())["fallbackReason"],
        "no_compatible_index"
    );
    for (workspace, vectors) in [
        (
            &first,
            json!([vector(&primary, 1.0, 1.0), vector(&alternate, 3.0, 0.0)]),
        ),
        (&second, json!([vector(&outside, 5.0, 0.0)])),
    ] {
        assert_eq!(
            peer.call(
                "workspace/memory/indexVectors",
                batch(workspace, vectors.clone())
            )["indexed"],
            vectors.as_array().unwrap().len()
        );
    }
    let zero = batch(&first, json!([vector(&primary, 0.0, 0.0)]));
    assert!(peer.request("workspace/memory/indexVectors", zero)["error"].is_object());
    let wrong_model = json!({"modelIdentity":"fixture-v2","values":[1,0]});
    assert_eq!(
        search(&mut peer, &first, wrong_model)["fallbackReason"],
        "no_compatible_index"
    );
    drop(peer);

    let mut peer = Peer::start(&db);
    let ranked = search(&mut peer, &first, query.clone());
    assert_eq!(ranked["mode"], "semantic");
    let items = ranked["items"].as_array().unwrap();
    assert_eq!(items.len(), 2);
    assert_eq!(items[0]["memory"]["id"], alternate["id"]);
    assert_eq!(items[0]["similarity"], 1.0);
    assert_eq!(items[1]["memory"]["id"], primary["id"]);
    assert_eq!(
        search(&mut peer, &second, query.clone())["items"][0]["memory"]["id"],
        outside["id"]
    );

    peer.call(
        "workspace/memory/delete",
        json!({"workspaceId":first["id"],"key":"review","expectedRevision":1}),
    );
    assert_eq!(
        search(&mut peer, &first, query.clone())["items"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    let stale = batch(&first, json!([vector(&alternate, 1.0, 0.0)]));
    assert!(peer.request("workspace/memory/indexVectors", stale)["error"].is_object());
    drop(peer);
    let sql = rusqlite::Connection::open(&db).unwrap();
    sql.execute(
        "UPDATE workspace_memory_vectors SET vector=zeroblob(length(vector)) WHERE memory_id=?1",
        [primary["id"].as_str().unwrap()],
    )
    .unwrap();
    drop(sql);

    let mut peer = Peer::start(&db);
    let corrupted = search(&mut peer, &first, query.clone());
    assert_eq!(corrupted["fallbackReason"], "corrupt_index");
    assert_eq!(
        corrupted["items"][0]["memory"]["content"],
        "Run Cargo before a release"
    );
    let listed = peer.call("workspace/memory/list", json!({"workspaceId":first["id"]}));
    assert_eq!(listed["items"].as_array().unwrap().len(), 1);
    peer.call(
        "workspace/memory/indexVectors",
        batch(&first, json!([vector(&listed["items"][0], 1.0, 0.0)])),
    );
    assert_eq!(search(&mut peer, &first, query)["mode"], "semantic");
    drop(peer);
    for file in fs::read_dir(&root).unwrap() {
        let file = file.unwrap();
        if file.file_type().unwrap().is_dir() {
            fs::remove_dir(file.path()).unwrap();
        } else {
            fs::remove_file(file.path()).unwrap();
        }
    }
    fs::remove_dir(root).unwrap();
}
