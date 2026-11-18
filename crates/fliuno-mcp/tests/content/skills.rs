use super::*;
use rmcp::model::{GetSkillResult, Skill, SkillFrontmatter, SkillResource, SkillResources};

const SKILL_URI: &str = "skill://fixture/report/SKILL.md";

fn capability(request: &wire::Request) -> Response {
    content::result(
        request,
        json!({"resultType":"complete","supportedVersions":["2026-07-28"],
        "capabilities":{"extensions":{"io.modelcontextprotocol/skills":{}}},
        "ttlMs":0,"cacheScope":"private"}),
    )
}

fn skill(uri: &str, name: &str) -> Skill {
    Skill::new(
        uri,
        SkillFrontmatter::new(name, "Build a report"),
        SkillResources::Dynamic,
    )
}

#[test]
fn advertised_skills_are_paginated_and_get_returns_only_metadata() {
    let fixture = Fixture::start(|request| match request.body["method"].as_str().unwrap() {
        "server/discover" => capability(request),
        "skills/list" => {
            if request.body["params"]["cursor"] == "more" {
                content::result(request, json!({"skills":[]}))
            } else {
                content::result(
                    request,
                    json!({"skills":[skill(SKILL_URI,"report")],"nextCursor":"more"}),
                )
            }
        }
        "skills/get" => {
            assert_eq!(request.body["params"]["uri"], SKILL_URI);
            let mut manifest = skill(SKILL_URI, "report");
            manifest.resources =
                SkillResources::Files(vec![SkillResource::from_bytes(SKILL_URI, b"report body")]);
            content::result(
                request,
                serde_json::to_value(GetSkillResult::new(manifest)).unwrap(),
            )
        }
        _ => content::reply(request),
    });
    let tools = connect(&fixture, 2000);
    assert_eq!(tools.len(), 2);
    assert_eq!(fixture.requests().len(), 1);
    let cancellation = CancellationToken::new();
    let list = tool(&tools, "skills_list");
    let page = invoke(list, json!({}), &cancellation).unwrap();
    assert_eq!(page.output["result"]["skills"][0]["uri"], SKILL_URI);
    assert_eq!(page.output["result"]["nextCursor"], "more");
    assert_eq!(count(&fixture, "skills/list"), 1);
    let next = invoke(list, json!({"cursor":"more"}), &cancellation).unwrap();
    assert_eq!(next.output["result"]["skills"], json!([]));
    let get = tool(&tools, "skills_get");
    assert!(
        get.validate(&json!({"uri":"skill://fixture/report/run.sh"}))
            .is_err()
    );
    assert!(get.validate(&json!({"uri":SKILL_URI,"_meta":{}})).is_err());
    assert_eq!(count(&fixture, "skills/get"), 0);
    let metadata = invoke(get, json!({"uri":SKILL_URI}), &cancellation).unwrap();
    assert_eq!(
        metadata.output["result"]["skill"]["frontmatter"]["name"],
        "report"
    );
    assert_eq!(
        metadata.output["result"]["skill"]["resources"],
        json!([SkillResource::from_bytes(SKILL_URI, b"report body")])
    );
    assert_eq!(metadata.output["grantsPermissions"], false);
    assert!(metadata.artifacts.is_empty());
    assert_eq!(count(&fixture, "skills/get"), 1);
    assert_eq!(count(&fixture, "resources/read"), 0);
}

#[test]
fn unrelated_or_invalid_metadata_is_rejected_without_resource_reads() {
    for (uri, name) in [
        ("skill://fixture/unrelated/SKILL.md", "unrelated"),
        (SKILL_URI, "wrong-name"),
    ] {
        let fixture =
            Fixture::start(
                move |request| match request.body["method"].as_str().unwrap() {
                    "server/discover" => capability(request),
                    "skills/get" => content::result(request, json!({"skill":skill(uri,name)})),
                    _ => content::reply(request),
                },
            );
        let tools = connect(&fixture, 2000);
        assert!(matches!(
            invoke(
                tool(&tools, "skills_get"),
                json!({"uri":SKILL_URI}),
                &CancellationToken::new()
            ),
            Err(ToolError::OutcomeUnknown(_))
        ));
        assert_eq!(count(&fixture, "skills/get"), 1);
        assert_eq!(count(&fixture, "resources/read"), 0);
    }
}
