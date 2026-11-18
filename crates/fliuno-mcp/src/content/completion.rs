//! Translate approved suggestions into one SDK request on the bound peer.
use super::arguments::{valid_arguments, valid_name, valid_uri};
use rmcp::model::{
    ArgumentInfo, ClientRequest, CompleteRequest, CompleteRequestParams, CompletionContext,
    CompletionInfo, Reference,
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::BTreeMap;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    r#ref: Target,
    argument: Argument,
    context: Option<Context>,
}

#[derive(Deserialize)]
#[serde(tag = "type", deny_unknown_fields)]
enum Target {
    #[serde(rename = "ref/prompt")]
    Prompt { name: String },
    #[serde(rename = "ref/resource")]
    Resource { uri: String },
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Argument {
    name: String,
    value: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Context {
    arguments: BTreeMap<String, String>,
}

pub(super) fn schema() -> Value {
    json!({
        "type":"object", "required":["ref","argument"], "additionalProperties":false,
        "properties": {
            "ref": {"anyOf":[
                {"type":"object", "required":["type","name"], "additionalProperties":false,
                 "properties":{"type":{"type":"string","enum":["ref/prompt"]},"name":{"type":"string","minLength":1,"maxLength":256}}},
                {"type":"object", "required":["type","uri"], "additionalProperties":false,
                 "properties":{"type":{"type":"string","enum":["ref/resource"]},"uri":{"type":"string","minLength":1,"maxLength":4096}}}
            ]},
            "argument": {"type":"object", "required":["name","value"], "additionalProperties":false,
                "properties":{"name":{"type":"string","minLength":1,"maxLength":256},
                               "value":{"type":"string","maxLength":8192}}},
            "context": {"type":"object", "required":["arguments"], "additionalProperties":false,
                "properties":{"arguments":{"type":"object","maxProperties":32,
                    "propertyNames":{"minLength":1,"maxLength":256},
                    "additionalProperties":{"type":"string","maxLength":8192}}}}
        }
    })
}

pub(super) fn request(value: &Value) -> Option<ClientRequest> {
    if value.get("context").is_some_and(Value::is_null) {
        return None;
    }
    let Input {
        r#ref,
        argument,
        context,
    } = serde_json::from_value(value.clone()).ok()?;
    let target = match r#ref {
        Target::Prompt { name } if valid_name(&name) => Reference::for_prompt(name),
        Target::Resource { uri } if valid_uri(&uri) => Reference::for_resource(uri),
        _ => return None,
    };
    if !valid_name(&argument.name) || argument.value.len() > 8192 {
        return None;
    }
    let mut params =
        CompleteRequestParams::new(target, ArgumentInfo::new(argument.name, argument.value));
    if let Some(Context { arguments }) = context {
        if !valid_arguments(&arguments) {
            return None;
        }
        params = params.with_context(CompletionContext::with_arguments(
            arguments.into_iter().collect(),
        ));
    }
    Some(ClientRequest::CompleteRequest(CompleteRequest::new(params)))
}

pub(super) fn within_budget(result: &Value) -> bool {
    result["completion"]["values"]
        .as_array()
        .is_some_and(|values| values.len() <= CompletionInfo::MAX_VALUES)
}
