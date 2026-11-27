//! Exercise the durable terminal collaboration workflows through stdio.
use super::*;

struct Directory(PathBuf);

impl Directory {
    fn new() -> Self {
        let path =
            std::env::temp_dir().join(format!("fliuno-collaboration-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
}

impl Drop for Directory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn plan(peer: &mut Peer, session: &Value, text: &str) -> Value {
    let task = peer.call(
        "task/start",
        json!({"sessionId":session,"input":text,
        "context":{"planOnly":true}}),
    );
    peer.wait_for_status(task["id"].as_str().unwrap(), "completed")
}

#[test]
fn pending_prompts_survive_restart_and_consume_in_order() {
    let root = Directory::new();
    let db = root.0.join("queue.db");
    let mut peer = Peer::start(&db);
    let workspace = peer.call("workspace/open", json!({"root":root.0}));
    let session = peer.call("session/create", json!({"workspaceId":workspace["id"]}));
    let other = peer.call("session/create", json!({"workspaceId":workspace["id"]}));
    let running = peer.call(
        "task/start",
        json!({"sessionId":session["id"],"input":"wait for approval"}),
    );
    peer.wait_for_status(running["id"].as_str().unwrap(), "waiting_approval");
    let draft = |key: &str, input: &str| {
        json!({"sessionId":session["id"],"input":input,
        "clientRequestId":key,"context":{"planOnly":true}})
    };
    let first = peer.call("taskQueue/enqueue", draft("a", "discover requirements"));
    let second = peer.call("taskQueue/enqueue", draft("b", "plan next iteration"));
    let removed = peer.call("taskQueue/enqueue", draft("c", "discarded"));
    assert_eq!(
        peer.call("taskQueue/enqueue", draft("a", "discover requirements")),
        first
    );
    assert!(
        peer.request("taskQueue/enqueue", draft("a", "different"))
            .get("error")
            .is_some()
    );
    let target = |item: &Value| json!({"sessionId":session["id"],"queueId":item["queueId"]});
    assert!(
        peer.request("taskQueue/startNext", target(&first))
            .get("error")
            .is_some()
    );
    assert!(
        peer.request(
            "taskQueue/startNext",
            json!({"sessionId":other["id"],"queueId":first["queueId"]})
        )
        .get("error")
        .is_some()
    );
    let cancelled = peer.call("taskQueue/cancel", target(&removed));
    assert_eq!(cancelled["status"]["state"], "cancelled");
    assert_eq!(peer.call("taskQueue/cancel", target(&removed)), cancelled);
    assert_eq!(
        peer.call("taskQueue/startNext", target(&removed)),
        cancelled
    );
    peer.call("task/cancel", json!({"taskId":running["id"]}));
    drop(peer);

    let mut peer = Peer::start(&db);
    let queue = peer.call("taskQueue/list", json!({"sessionId":session["id"]}));
    assert!(queue["blockingTaskId"].is_null());
    assert_eq!(queue["pending"], json!([first, second]));
    assert!(
        peer.request("taskQueue/startNext", target(&second))
            .get("error")
            .is_some()
    );
    let connection = rusqlite::Connection::open(&db).unwrap();
    connection
        .execute_batch(
            "CREATE TRIGGER fail_dispatch BEFORE UPDATE OF task_id ON task_queue
         BEGIN SELECT RAISE(ABORT,'test dispatch rollback'); END;",
        )
        .unwrap();
    let before = peer.call("events/getWatermark", json!({}));
    assert!(
        peer.request("taskQueue/startNext", target(&first))
            .get("error")
            .is_some()
    );
    assert_eq!(peer.call("events/getWatermark", json!({})), before);
    assert_eq!(
        peer.call("taskQueue/list", json!({"sessionId":session["id"]})),
        queue
    );
    connection
        .execute_batch("DROP TRIGGER fail_dispatch")
        .unwrap();
    drop(connection);
    let started = peer.call("taskQueue/startNext", target(&first));
    let task = peer.wait_for_status(started["status"]["taskId"].as_str().unwrap(), "completed");
    assert_eq!(task["context"]["options"]["planOnly"], true);
    assert_eq!(task["messages"][0]["text"], "discover requirements");
    drop(peer);

    let mut peer = Peer::start(&db);
    assert_eq!(peer.call("taskQueue/startNext", target(&first)), started);
    let started_second = peer.call("taskQueue/startNext", target(&second));
    assert_ne!(
        started["status"]["taskId"],
        started_second["status"]["taskId"]
    );
    assert_eq!(
        peer.call("taskQueue/list", json!({"sessionId":session["id"]}))["pending"],
        json!([])
    );
}

#[test]
fn session_forks_freeze_conversation_and_have_their_own_timeline() {
    let root = Directory::new();
    let db = root.0.join("branch.db");
    let mut peer = Peer::start(&db);
    let workspace = peer.call("workspace/open", json!({"root":root.0}));
    let session = peer.call(
        "session/create",
        json!({"workspaceId":workspace["id"],"title":"Main branch"}),
    );
    let first = plan(&mut peer, &session["id"], "original plan");
    let newer = plan(&mut peer, &session["id"], "later plan");
    let request = json!({"sessionId":session["id"],"throughTaskId":first["id"],"title":"Alternative branch","clientRequestId":"fork"});
    let branch = peer.call("session/fork", request.clone());
    assert_eq!(branch["workspaceId"], workspace["id"]);
    assert_eq!(branch["forkOrigin"]["inheritedTurns"], 1);
    assert_ne!(branch["id"], session["id"]);
    assert_eq!(peer.call("session/fork", request.clone()), branch);
    let own_tasks = peer.call("task/listSummaries", json!({"sessionId":branch["id"]}));
    assert_eq!(own_tasks["items"], json!([]));
    let context = peer.call("session/context", json!({"sessionId":branch["id"]}));
    let inherited = context["turns"].as_array().unwrap();
    assert_eq!(inherited.len(), 1);
    assert_eq!(inherited[0]["taskId"], first["id"]);
    assert_ne!(inherited[0]["taskId"], newer["id"]);
    assert_eq!(
        peer.call("session/listHistory", json!({"sessionId":branch["id"]}))["entries"][0]["turn"]["taskId"],
        first["id"]
    );
    let follow = plan(&mut peer, &branch["id"], "branch plan");
    let page = peer.call("task/timeline", json!({"taskId":follow["id"]}));
    let entries = page["entries"].as_array().unwrap();
    assert_eq!(entries[0]["kind"], "task_started");
    assert_eq!(entries.last().unwrap()["kind"], "task_completed");
    let first_event = peer.call("events/get", json!({"sequence":entries[0]["sequence"]}));
    assert_eq!(first_event["taskId"], follow["id"]);
    let tail = peer.call("task/timeline", json!({"taskId":follow["id"],"after":entries[0]["sequence"], "throughSequence":page["throughSequence"]}));
    assert_eq!(tail["entries"].as_array().unwrap().len(), entries.len() - 1);
    let search = json!({"workspaceId":workspace["id"],"query":"ALTERNATIVE","archived":false});
    assert_eq!(
        peer.call("session/list", search.clone())["items"],
        json!([branch])
    );
    peer.call(
        "session/setArchived",
        json!({"sessionId":branch["id"],"archived":true}),
    );
    assert_eq!(peer.call("session/list", search)["items"], json!([]));
    drop(peer);
    let mut peer = Peer::start(&db);
    assert_eq!(peer.call("session/fork", request)["id"], branch["id"]);
    assert_eq!(
        peer.call("session/context", json!({"sessionId":branch["id"]}))["turns"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
}
