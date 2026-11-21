//! Resource/prompt wire peer without tools. This catches accidental eager
//! tools/list and provides explicit cache hints for reuse and expiry tests.
use super::wire::{Request, Response};
use serde_json::{Value, json};

pub fn result(request: &Request, result: Value) -> Response {
    Response::json(
        200,
        json!({"jsonrpc":"2.0","id":request.body["id"],"result":result}),
    )
}
pub fn reply(request: &Request) -> Response {
    reply_with_ttl(request, 60_000)
}

pub fn reply_with_ttl(request: &Request, ttl_ms: u64) -> Response {
    if request.method == "GET" {
        return Response::json(405, Value::Null);
    }
    if request.method == "DELETE" {
        return Response::json(200, Value::Null);
    }
    let mut value = match request.body["method"].as_str().unwrap() {
        "server/discover" => json!({"resultType":"complete","supportedVersions":["2026-07-28"],
            "capabilities":{"resources":{},"prompts":{},"completions":{}},"ttlMs":0,"cacheScope":"private"}),
        "resources/list" => {
            if request.body["params"]["cursor"] == "second" {
                json!({"resources":[{"name":"second","uri":"fixture:second"}],"ttlMs":60000,"cacheScope":"private"})
            } else {
                json!({"resources":[{"name":"first","uri":"fixture:first"}],"nextCursor":"second","ttlMs":60000,"cacheScope":"private"})
            }
        }
        "resources/templates/list" => {
            json!({"resourceTemplates":[{"name":"report","uriTemplate":"fixture:report/{name}"}],"ttlMs":60000,"cacheScope":"private"})
        }
        "resources/read" => json!({"contents":[
            {"uri":request.body["params"]["uri"],"mimeType":"text/plain","text":"REMOTE_CONTENT_ONLY"},
            {"uri":"fixture:binary","mimeType":"application/octet-stream","blob":"AAH/"}
        ],"ttlMs":60000,"cacheScope":"private"}),
        "prompts/list" => {
            json!({"prompts":[{"name":"draft","arguments":[{"name":"topic","required":true}]}],"ttlMs":60000,"cacheScope":"private"})
        }
        "prompts/get" => json!({"messages":[
            {"role":"user","content":{"type":"text","text":"UNTRUSTED_PROMPT_CONTENT"}},
            {"role":"assistant","content":{"type":"text","text":request.body["params"]["arguments"]["topic"].as_str().unwrap_or_default()}}
        ]}),
        "completion/complete" => json!({"resultType":"complete","completion":{
            "values":["项目报告","项目简报"],"total":3,"hasMore":true
        }}),
        "notifications/cancelled" => return Response::json(202, Value::Null),
        other => panic!("unexpected MCP content request {other}"),
    };
    if request.body["method"] != "server/discover" && value.get("ttlMs").is_some() {
        value["ttlMs"] = json!(ttl_ms);
    }
    result(request, value)
}
