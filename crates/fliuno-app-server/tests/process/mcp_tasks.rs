use super::*;
use http_wire::{Fixture, Response, Version};
use std::sync::atomic::{AtomicUsize, Ordering};

#[test]
fn remote_task_answer_is_not_resent_after_app_server_is_killed_during_dispatch() {
    let (sent, received) = mpsc::channel();
    let model_turn = AtomicUsize::new(0);
    let task_id = "remote-task";
    let fixture = Fixture::start(move |request| {
        if request.path == "/v1/responses" {
            let turn = model_turn.fetch_add(1, Ordering::Relaxed);
            let output = match turn {
                0 | 2 | 3 => {
                    let (name, arguments) = if turn == 0 {
                        let name = request.body["tools"]
                            .as_array()
                            .unwrap()
                            .iter()
                            .find_map(|t| {
                                t["name"]
                                    .as_str()
                                    .filter(|name| name.starts_with("mcp_remote_start_"))
                            })
                            .unwrap();
                        (name, json!({}))
                    } else {
                        (
                            "mcp_remote_tasks_update",
                            json!({"taskId":task_id,"inputResponses":{
                                "form":{"action":"accept","content":{}}
                            }}),
                        )
                    };
                    json!([{"type":"function_call","call_id":format!("call-{turn}"),"name":name,"arguments":arguments.to_string()}])
                }
                1 => {
                    json!([{"type":"message","content":[{"type":"output_text","text":"Created"}]}])
                }
                _ => panic!("unexpected model turn {turn}"),
            };
            return Response::json(200, json!({"status":"completed","output":output}));
        }
        if request.body["method"] == "tasks/update" {
            let _ = sent.send(());
            return Response::hold();
        }
        let mut task = json!({"taskId":task_id,"status":"working","createdAt":"2026-11-21T00:00:00Z",
            "lastUpdatedAt":"2026-11-21T00:00:00Z","ttlMs":null});
        let result = match request.body["method"].as_str() {
            Some("server/discover") => {
                json!({"resultType":"complete","supportedVersions":["2026-07-28"],
                "ttlMs":0,"cacheScope":"private","capabilities":{"tools":{},"extensions":{"io.modelcontextprotocol/tasks":{}}}})
            }
            Some("tools/list") => {
                json!({"tools":[{"name":"start","inputSchema":{"type":"object"}}]})
            }
            Some("tools/call") => {
                task["resultType"] = json!("task");
                task
            }
            Some("tasks/get") => {
                task["resultType"] = json!("complete");
                task["status"] = json!("input_required");
                task["inputRequests"] = json!({"form":{"method":"elicitation/create", "params":{
                    "mode":"form","message":"Continue?","requestedSchema":{"type":"object","properties":{}}
                }}});
                task
            }
            _ => return http_wire::reply(request, Version::Modern),
        };
        Response::json(
            200,
            json!({"jsonrpc":"2.0","id":request.body["id"],"result":result}),
        )
    });

    let root = std::env::temp_dir().join(format!("fliuno-mcp-restart-{}", uuid::Uuid::new_v4()));
    fs::create_dir(&root).unwrap();
    let config = root.join("mcp.json");
    fs::write(
        &config,
        json!({"servers":[{"id":"remote","transport":"streamable_http",
        "url":fixture.url("/mcp"),"timeoutMs":15000}]})
        .to_string(),
    )
    .unwrap();
    let db = root.join("kernel.db");
    let base = fixture.url("/v1/");
    let start = || Peer::start_with_remote_tools(&db, Some(&base), Some(&config));
    let mut peer = start();
    let workspace = peer.call("workspace/open", json!({"root":root}));
    let session = peer.call("session/create", json!({"workspaceId":workspace["id"]}));
    let approve = |peer: &mut Peer, text, prefix: &str| {
        let task = peer.call(
            "task/start",
            json!({"sessionId":session["id"],"input":text}),
        );
        let id = task["id"].as_str().unwrap().to_owned();
        let waiting = peer.wait_for_status(&id, "waiting_approval");
        let approval = &waiting["pendingApproval"];
        assert!(
            approval["call"]["name"]
                .as_str()
                .unwrap()
                .starts_with(prefix)
        );
        peer.call(
            "approval/respond",
            json!({"taskId":id,"approvalId":approval["id"],"decision":"allow_once"}),
        );
        id
    };
    let create = approve(&mut peer, "create remote task", "mcp_remote_start_");
    peer.wait_for_status(&create, "completed");
    let update = approve(&mut peer, "answer remote form", "mcp_remote_tasks_update");
    received.recv_timeout(Duration::from_secs(5)).unwrap();
    peer.child.kill().unwrap();
    drop(peer);

    let mut peer = start();
    let recovered = peer.call("task/get", json!({"taskId":update}));
    assert_eq!(recovered["status"], "interrupted");
    assert!(recovered["inFlightTool"].is_object());
    let retry = approve(
        &mut peer,
        "answer the same remote form again",
        "mcp_remote_tasks_update",
    );
    let rejected = peer.wait_for_status(&retry, "failed");
    assert!(
        rejected["output"]
            .as_str()
            .unwrap()
            .contains("already sent")
    );
    for method in ["tools/call", "tasks/update"] {
        assert_eq!(
            fixture
                .requests()
                .iter()
                .filter(|request| request.body["method"] == method)
                .count(),
            1
        );
    }
    drop(peer);
    for file in fs::read_dir(&root).unwrap() {
        fs::remove_file(file.unwrap().path()).unwrap();
    }
    fs::remove_dir(root).unwrap();
}
