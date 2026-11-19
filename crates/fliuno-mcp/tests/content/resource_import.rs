use super::*;

#[test]
fn rejects_invalid_and_oversized_imports_without_fallback_or_partial_results() {
    let wire = Fixture::start(|request| {
        if request.body["method"] != "resources/read" {
            return content::reply(request);
        }
        let part = || json!({"uri":"fixture:txt","text":"kept only on success"});
        let large_text = |size| json!({"uri":"fixture:body","text":"x".repeat(size)});
        let limit = fliuno_protocol::MAX_BINARY_ARTIFACT_BYTES;
        let parts = match request.body["params"]["uri"].as_str().unwrap() {
            "test:invalid-base64" => vec![part(), json!({"uri":"fixture:binary","blob":"b@d="})],
            "test:invalid-uri" => vec![part(), json!({"uri":"no valid scheme","blob":"AAE="})],
            "test:empty" => vec![],
            "test:too-many" => vec![part(); 17],
            "test:oversized-part" => vec![part(), large_text(limit + 1)],
            "test:over-total" => vec![large_text(limit), large_text(limit), part()],
            "test:exact" => vec![large_text(limit), large_text(limit)],
            other => panic!("unexpected URI: {other}"),
        };
        content::result(request, json!({"resultType":"complete","contents":parts}))
    });
    let tools = connect(&wire, 3000);
    let import = tool(&tools, "resources_import");
    let cancellation = CancellationToken::new();
    for args in [
        json!({"uri":"file:/fixture","title":"\n"}),
        json!({"uri":"file:/fixture","title":"x".repeat(241)}),
        json!({"uri":"file:/fixture","title":"ok","_meta":{}}),
    ] {
        assert!(import.validate(&args).is_err());
        assert!(matches!(
            invoke(import, args, &cancellation),
            Err(ToolError::Failed(_))
        ));
    }
    assert_eq!(count(&wire, "resources/read"), 0);
    for uri in [
        "test:invalid-base64",
        "test:invalid-uri",
        "test:empty",
        "test:too-many",
        "test:oversized-part",
        "test:over-total",
    ] {
        assert!(
            matches!(
                invoke(import, json!({"uri":uri,"title":"Import"}), &cancellation),
                Err(ToolError::Failed(_))
            ),
            "accepted {uri}"
        );
    }
    let accepted = invoke(
        import,
        json!({"uri":"test:exact","title":"Exact"}),
        &cancellation,
    )
    .unwrap();
    assert_eq!(accepted.artifacts.len(), 2);
    assert_eq!(accepted.artifacts[0].body.as_bytes().len(), 256 * 1024);
    assert_eq!(accepted.artifacts[1].body.as_bytes().len(), 256 * 1024);
    assert_eq!(
        accepted.output["result"]["contents"][0]["artifactId"],
        accepted.artifacts[0].id
    );
    assert!(
        accepted.output["result"]["contents"][0]
            .get("text")
            .is_none()
    );
    assert_eq!(count(&wire, "resources/read"), 7);
}
