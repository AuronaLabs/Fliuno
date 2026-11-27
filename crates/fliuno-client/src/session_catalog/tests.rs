use super::*;
use serde_json::{Value, json};
use std::io::Cursor;

#[test]
fn malformed_catalog_pages_keep_the_continuation_and_legacy_order_omits_the_field() {
    let item = json!({"id":"s1","workspaceId":"ws"});
    let foreign = json!({"id":"s2","workspaceId":"other"});
    let archived = json!({"id":"s3","workspaceId":"ws","archived":true});
    let replies = [
        json!({"items":[foreign],"nextAfter":12,"hasMore":false}),
        json!({"items":[archived],"nextAfter":12,"hasMore":false}),
        json!({"items":[item,item],"nextAfter":12,"hasMore":true}),
        json!({"items":[item],"nextAfter":18,"hasMore":false}),
        json!({"items":[],"nextAfter":17,"hasMore":true}),
        json!({"items":[item],"nextAfter":12,"hasMore":false}),
    ];
    let mut input = b"{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"protocolVersion\":\"0.1\",\"serverName\":\"test\",\"serverVersion\":\"0\",\"capabilities\":{}}}\n".to_vec();
    for (i, result) in replies.into_iter().enumerate() {
        serde_json::to_writer(
            &mut input,
            &json!({"jsonrpc":"2.0","id":i+2,"result":result}),
        )
        .unwrap();
        input.push(b'\n');
    }
    let mut client =
        Client::connect(Cursor::new(input), Vec::new(), "catalog-test", |_| {}).unwrap();
    let mut catalog = SessionCatalog::new(SessionListParams {
        workspace_id: "ws".into(),
        archived: Some(false),
        after: 17,
        limit: 2,
        order: Some(SessionListOrder::NewestFirst),
        query: None,
    });
    for _ in 0..5 {
        assert!(matches!(
            catalog.next_page(&mut client, |_| {}),
            Err(Error::InvalidSessionCatalogPage)
        ));
        assert_eq!(catalog.next_request().unwrap().after, 17);
    }
    assert_eq!(
        catalog
            .next_page(&mut client, |_| {})
            .unwrap()
            .unwrap()
            .next_after,
        12
    );
    assert!(catalog.next_request().is_none());
    assert!(catalog.next_page(&mut client, |_| {}).unwrap().is_none());

    let legacy = SessionCatalog::new(SessionListParams {
        workspace_id: "ws".into(),
        archived: None,
        after: 0,
        limit: 2,
        order: Some(SessionListOrder::OldestFirst),
        query: None,
    });
    let encoded: Value = serde_json::to_value(legacy.next_request().unwrap()).unwrap();
    assert!(encoded.get("order").is_none());
}
