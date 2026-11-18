use super::*;

#[test]
fn completion_sends_only_the_approved_reference_argument_and_context() {
    let fixture = Fixture::start(content::reply);
    let tools = connect(&fixture, 2000);
    let tool = tool(&tools, "complete");
    let prompt = json!({
        "ref":{"type":"ref/prompt","name":"draft"},
        "argument":{"name":"topic","value":"项目"},
        "context":{"arguments":{"language":"中文"}}
    });
    let resource = json!({
        "ref":{"type":"ref/resource","uri":"fixture:report/{id}"},
        "argument":{"name":"id","value":""}
    });
    for (pointer, replacement) in [
        (
            "/ref",
            json!({"type":"ref/prompt","name":"draft","title":"override"}),
        ),
        ("/ref", json!({"type":"ref/resource","uri":"../report"})),
        (
            "/argument",
            json!({"name":"topic","value":"界".repeat(2731)}),
        ),
        ("/context", json!({"arguments":{"language":123}})),
        ("/context", json!(null)),
    ] {
        let mut invalid = prompt.clone();
        *invalid.pointer_mut(pointer).unwrap() = replacement;
        assert!(invoke(tool, invalid, &CancellationToken::new()).is_err());
    }
    assert_eq!(fixture.requests().len(), 1);
    for arguments in [&prompt, &resource] {
        let result = invoke(tool, arguments.clone(), &CancellationToken::new()).unwrap();
        assert_eq!(result.output["method"], "completion/complete");
        assert_eq!(result.output["grantsPermissions"], false);
        assert_eq!(
            result.output["result"]["completion"],
            json!({
                "values":["项目报告","项目简报"],"total":3,"hasMore":true
            })
        );
        assert!(result.artifacts.is_empty());
    }
    let requests = fixture.requests();
    assert_eq!(requests.len(), 3);
    for (request, input) in requests.iter().skip(1).zip([prompt, resource]) {
        assert_eq!(request.body["method"], "completion/complete");
        let mut params = request.body["params"].as_object().unwrap().clone();
        let meta = params.remove("_meta").unwrap();
        assert_eq!(
            meta["io.modelcontextprotocol/protocolVersion"],
            "2026-07-28"
        );
        assert_eq!(Value::Object(params), input);
    }
}

#[test]
fn oversized_or_incomplete_completion_does_not_trigger_followup_requests() {
    for response in [
        json!({"resultType":"complete","completion":{"values":vec!["x";101]}}),
        json!({"resultType":"input_required","completion":{"values":["must not be accepted"]}}),
    ] {
        let fixture = Fixture::start(move |request| {
            if request.body["method"] == "completion/complete" {
                content::result(request, response.clone())
            } else {
                content::reply(request)
            }
        });
        let tools = connect(&fixture, 2000);
        let result = invoke(
            tool(&tools, "complete"),
            json!({"ref":{"type":"ref/prompt","name":"draft"},"argument":{"name":"topic","value":""}}),
            &CancellationToken::new(),
        );
        assert!(matches!(result, Err(ToolError::OutcomeUnknown(_))));
        assert_eq!(count(&fixture, "completion/complete"), 1);
        assert_eq!(count(&fixture, "prompts/get"), 0);
        assert_eq!(count(&fixture, "resources/read"), 0);
    }
}
