use super::*;
use fliuno_client::{TaskSummaryCatalog, TaskSummaryCatalogRequest};
use fliuno_protocol::{TaskSummaryListParams, TaskSummaryOrder, WorkspaceTaskSummaryListParams};

#[test]
fn newest_session_catalog_resumes_by_creation_position_and_keeps_its_live_filter() {
    let root = std::env::temp_dir().join(format!("fliuno-task-catalog-{}", uuid::Uuid::new_v4()));
    let foreign_root = root.join("foreign");
    fs::create_dir_all(&foreign_root).unwrap();
    let db = root.join("kernel.db");
    let mut peer = Peer::start(&db);
    let ws = peer.call("workspace/open", json!({"root":root}));
    let foreign_ws = peer.call("workspace/open", json!({"root":foreign_root}));
    let create = |peer: &mut Peer, ws: &Value| {
        peer.call("session/create", json!({"workspaceId":ws["id"]}))["id"]
            .as_str()
            .unwrap()
            .to_owned()
    };
    let session = create(&mut peer, &ws);
    let sibling = create(&mut peer, &ws);
    let foreign = create(&mut peer, &foreign_ws);
    let start = |peer: &mut Peer, session: &str| {
        let task = peer.call(
            "task/start",
            json!({"sessionId":session,"input":"文档".repeat(100)}),
        );
        let id = task["id"].as_str().unwrap().to_owned();
        peer.wait_for_status(&id, "waiting_approval");
        id
    };
    let oldest = start(&mut peer, &session);
    let sibling_task = start(&mut peer, &sibling);
    let excluded = start(&mut peer, &session);
    peer.call("task/cancel", json!({"taskId":excluded}));
    let middle = start(&mut peer, &session);
    let newest = start(&mut peer, &session);
    let oldest_page = peer.call("task/listSummaries", json!({"sessionId":session,"limit":1}));
    assert_eq!(oldest_page["items"][0]["id"], oldest);
    assert_eq!(
        peer.call(
            "task/listSummaries",
            json!({"sessionId":session,"limit":1,"order":"oldest_first"})
        ),
        oldest_page
    );
    assert_eq!(
        peer.call(
            "task/listSummaries",
            json!({"sessionId":session,"limit":1,"order":"newest_first"})
        )["items"][0]["id"],
        newest
    );
    assert_eq!(
        peer.call(
            "task/listSummaries",
            json!({"sessionId":session,"after":1,"order":"newest_first"})
        ),
        json!({"items":[],"nextAfter":1,"hasMore":false})
    );
    for params in [
        json!({"sessionId":session,"order":"invalid"}),
        json!({"sessionId":session,"order":"newest_first","after":u64::MAX}),
    ] {
        assert_eq!(
            peer.request("task/listSummaries", params)["error"]["code"],
            -32602
        );
    }
    drop(peer);

    let (server, mut client) = super::ag_ui_run::connect(&db);
    assert_eq!(
        client.info().capabilities["sessionTaskSummaryOrdering"],
        true
    );
    let request = TaskSummaryListParams {
        session_id: session.clone(),
        status: Some(TaskStatus::WaitingApproval),
        after: 0,
        limit: 3,
        order: Some(TaskSummaryOrder::NewestFirst),
        target_bytes: Some(1024),
    };
    let mut catalog = TaskSummaryCatalog::new(TaskSummaryCatalogRequest::Session(request.clone()));
    let head = catalog.next_page(&mut client, |_| {}).unwrap().unwrap();
    assert_eq!(head.items.len(), 1); // Byte budget, before the row limit.
    assert_eq!(head.items[0].id, newest);
    assert!(head.has_more);
    let saved = serde_json::to_vec(catalog.next_request().unwrap()).unwrap();
    drop(client);
    drop(server);

    let mut peer = Peer::start(&db);
    let later = start(&mut peer, &session);
    let foreign_task = start(&mut peer, &foreign);
    peer.call("task/cancel", json!({"taskId":middle}));
    drop(peer);
    let (server, mut client) = super::ag_ui_run::connect(&db);
    let mut catalog = TaskSummaryCatalog::new(serde_json::from_slice(&saved).unwrap());
    let tail = catalog.next_page(&mut client, |_| {}).unwrap().unwrap();
    assert_eq!(tail.items.len(), 1);
    assert_eq!(tail.items[0].id, oldest);
    assert!(tail.next_after < head.next_after);
    assert!(!tail.has_more);
    assert!(catalog.next_request().is_none());
    assert!(catalog.next_page(&mut client, |_| {}).unwrap().is_none());
    let mut current = TaskSummaryCatalog::new(TaskSummaryCatalogRequest::Session(request));
    assert_eq!(
        current
            .next_page(&mut client, |_| {})
            .unwrap()
            .unwrap()
            .items[0]
            .id,
        later
    );

    assert_eq!(
        client.info().capabilities["workspaceTaskSummaryOrdering"],
        true
    );
    let mut workspace = TaskSummaryCatalog::new(TaskSummaryCatalogRequest::Workspace(
        WorkspaceTaskSummaryListParams {
            workspace_id: ws["id"].as_str().unwrap().into(),
            order: Some(TaskSummaryOrder::OldestFirst),
            after: 0,
            limit: 4,
            target_bytes: None,
            status: Some(TaskStatus::WaitingApproval),
        },
    ));
    assert!(
        serde_json::to_value(workspace.next_request()).unwrap()["params"]
            .get("order")
            .is_none()
    );
    let page = workspace.next_page(&mut client, |_| {}).unwrap().unwrap();
    let ids: Vec<_> = page.items.iter().map(|task| task.id.as_str()).collect();
    assert_eq!(ids, [&oldest, &sibling_task, &newest, &later]);
    assert!(!ids.contains(&foreign_task.as_str()));
    assert!(!page.has_more);
    drop(client);
    drop(server);
    fs::remove_dir_all(root).unwrap();
}
