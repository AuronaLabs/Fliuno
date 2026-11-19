use super::*;
use http_wire::{Fixture, Response};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

#[test]
fn workspace_search_requires_consent_and_returns_bounded_positions() {
    let root = std::env::temp_dir()
        .canonicalize()
        .unwrap()
        .join(format!("fliuno-workspace-search-{}", uuid::Uuid::new_v4()));
    fs::create_dir(&root).unwrap();
    fs::write(root.join("a.txt"), "not present yet\n").unwrap();
    fs::write(root.join("b.txt"), "预算\n").unwrap();
    fs::write(root.join("none.txt"), "ordinary text\n").unwrap();
    fs::write(root.join("binary.dat"), b"\xff\x00").unwrap();
    fs::write(root.join("too-large.txt"), vec![b'a'; 256 * 1024 + 1]).unwrap();
    fs::create_dir(root.join("directory")).unwrap();

    let args = json!({
        "paths":["a.txt","b.txt","none.txt","binary.dat","too-large.txt","missing.txt","directory"],
        "query":"预算","maxMatches":2
    });
    let args_json = args.to_string();
    let expected = json!({"files":[
        {"path":"a.txt","status":"searched","matches":[{"line":2,"byteColumn":5},{"line":3,"byteColumn":1}],"hasMore":false},
        {"path":"b.txt","status":"searched","matches":[],"hasMore":true},
        {"path":"none.txt","status":"searched","matches":[],"hasMore":false},
        {"path":"binary.dat","status":"nonText"},
        {"path":"too-large.txt","status":"unreadable"},
        {"path":"missing.txt","status":"unreadable"},
        {"path":"directory","status":"unreadable"}
    ]});
    let results = Arc::new(AtomicUsize::new(0));
    let observed_results = Arc::clone(&results);
    let fixture = Fixture::start(move |request| {
        let input = request.body["input"].as_array().unwrap();
        let output = if let Some(result) = input
            .iter()
            .find(|value| value["type"] == "function_call_output")
        {
            let returned: Value = serde_json::from_str(result["output"].as_str().unwrap()).unwrap();
            assert_eq!(returned, expected);
            observed_results.fetch_add(1, Ordering::Relaxed);
            json!([{"type":"message","content":[{"type":"output_text","text":"located"}]}])
        } else {
            let user = input.iter().find(|value| value["role"] == "user").unwrap();
            let arguments = if user.to_string().contains("outside path") {
                json!({"paths":["../outside.txt"],"query":"预算"}).to_string()
            } else {
                args_json.clone()
            };
            json!([{"type":"function_call","call_id":"search","name":"workspace_search_text","arguments":arguments}])
        };
        Response::json(200, json!({"status":"completed","output":output}))
    });
    let mut peer =
        Peer::start_with_model_fixture(&root.join("kernel.db"), Some(&fixture.url("/v1/")));
    let workspace = peer.call("workspace/open", json!({"root":root}));
    let tool_specs = peer.call("tools/list", json!({}));
    let spec = tool_specs
        .as_array()
        .unwrap()
        .iter()
        .find(|spec| spec["name"] == "workspace_search_text")
        .unwrap();
    assert_eq!(spec["effect"], "workspace_read");
    for (input, decision, outcome) in [
        ("outside path", None, "failed"),
        ("find mentions", Some("deny"), "failed"),
        ("find mentions", Some("allow_once"), "completed"),
    ] {
        let session = peer.call("session/create", json!({"workspaceId":workspace["id"]}));
        let task = peer.call(
            "task/start",
            json!({"sessionId":session["id"],"input":input}),
        );
        let id = task["id"].as_str().unwrap();
        if let Some(decision) = decision {
            let waiting = peer.wait_for_status(id, "waiting_approval");
            assert_eq!(waiting["pendingApproval"]["call"]["arguments"], args);
            assert_eq!(results.load(Ordering::Relaxed), 0);
            if decision == "allow_once" {
                // The approved read uses current bytes, including CRLF and multibyte text.
                fs::write(
                    root.join("a.txt"),
                    "no hit\r\n界 预算预算 and undisclosed source\r\n预算",
                )
                .unwrap();
            }
            peer.call(
                "approval/respond",
                json!({
                    "taskId":id,"approvalId":waiting["pendingApproval"]["id"],"decision":decision
                }),
            );
        }
        let finished = peer.wait_for_status(id, outcome);
        assert!(finished["pendingApproval"].is_null());
        assert_eq!(
            results.load(Ordering::Relaxed),
            usize::from(outcome == "completed")
        );
    }
    drop(peer);
    fs::remove_dir_all(root).unwrap();
}
