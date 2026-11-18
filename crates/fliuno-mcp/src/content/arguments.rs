//! Fixed method selection and strict argument budgets: model input can choose
//! content on this peer, never another endpoint, headers, metadata or MRTR state.
use super::Kind;
use rmcp::model::{
    ClientRequest, GetPromptRequest, GetPromptRequestParams, GetSkillRequest,
    GetSkillRequestParams, ListPromptsRequest, ListResourceTemplatesRequest, ListResourcesRequest,
    ListSkillsRequest, PaginatedRequestParams, ReadResourceRequest, ReadResourceRequestParams,
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::BTreeMap;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Page {
    cursor: Option<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Resource {
    uri: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Prompt {
    name: String,
    #[serde(default)]
    arguments: BTreeMap<String, String>,
}
pub(super) fn valid_cursor(cursor: &str) -> bool {
    !cursor.is_empty() && cursor.len() <= 4096 && !cursor.chars().any(char::is_control)
}
pub(super) fn valid_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= 256 && !name.chars().any(char::is_control)
}
pub(super) fn valid_uri(uri: &str) -> bool {
    let Some((scheme, _)) = uri.split_once(':') else {
        return false;
    };
    uri.len() <= 4096
        && !uri.chars().any(|c| c.is_control() || c.is_whitespace())
        && scheme
            .as_bytes()
            .first()
            .is_some_and(u8::is_ascii_alphabetic)
        && scheme
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"+.-".contains(&byte))
}
impl Kind {
    pub(super) fn schema(self) -> Value {
        let (required, properties) = match self {
            Self::Resources | Self::Templates | Self::Prompts | Self::SkillsList => (
                vec![],
                json!({
                    "cursor":{"type":"string","minLength":1,"maxLength":4096}
                }),
            ),
            Self::Read | Self::SkillGet => (
                vec!["uri"],
                json!({
                    "uri":{"type":"string","minLength":1,"maxLength":4096}
                }),
            ),
            Self::Get => (
                vec!["name"],
                json!({
                    "name":{"type":"string","minLength":1,"maxLength":256},
                    "arguments":{"type":"object","maxProperties":32,
                        "propertyNames":{"minLength":1,"maxLength":256},
                        "additionalProperties":{"type":"string","maxLength":8192}}
                }),
            ),
            Self::Complete => return super::completion::schema(),
        };
        json!({"type":"object","additionalProperties":false,"required":required,"properties":properties})
    }

    pub(super) fn request(self, value: &Value) -> Result<ClientRequest, String> {
        let invalid = || "invalid MCP content arguments or argument budget".to_owned();
        if value.to_string().len() > 65_536 {
            return Err(invalid());
        }
        Ok(match self {
            Self::Resources | Self::Templates | Self::Prompts | Self::SkillsList => {
                let args: Page = serde_json::from_value(value.clone()).map_err(|_| invalid())?;
                if value.get("cursor").is_some_and(|v| !v.is_string())
                    || args
                        .cursor
                        .as_ref()
                        .is_some_and(|cursor| !valid_cursor(cursor))
                {
                    return Err(invalid());
                }
                let params = PaginatedRequestParams::default().with_cursor(args.cursor);
                match self {
                    Self::Resources => ClientRequest::ListResourcesRequest(
                        ListResourcesRequest::with_param(params),
                    ),
                    Self::Templates => ClientRequest::ListResourceTemplatesRequest(
                        ListResourceTemplatesRequest::with_param(params),
                    ),
                    Self::SkillsList => {
                        ClientRequest::ListSkillsRequest(ListSkillsRequest::with_param(params))
                    }
                    _ => ClientRequest::ListPromptsRequest(ListPromptsRequest::with_param(params)),
                }
            }
            Self::Read | Self::SkillGet => {
                let args: Resource =
                    serde_json::from_value(value.clone()).map_err(|_| invalid())?;
                if !valid_uri(&args.uri) {
                    return Err(invalid());
                }
                if matches!(self, Self::SkillGet) {
                    if rmcp::model::skill_name_from_uri(&args.uri).is_none() {
                        return Err(invalid());
                    }
                    ClientRequest::GetSkillRequest(GetSkillRequest::new(
                        GetSkillRequestParams::new(args.uri),
                    ))
                } else {
                    ClientRequest::ReadResourceRequest(ReadResourceRequest::new(
                        ReadResourceRequestParams::new(args.uri),
                    ))
                }
            }
            Self::Get => {
                let args: Prompt = serde_json::from_value(value.clone()).map_err(|_| invalid())?;
                if !valid_name(&args.name) || !valid_arguments(&args.arguments) {
                    return Err(invalid());
                }
                let arguments = args
                    .arguments
                    .into_iter()
                    .map(|(key, value)| (key, Value::String(value)))
                    .collect();
                ClientRequest::GetPromptRequest(GetPromptRequest::new(
                    GetPromptRequestParams::new(args.name).with_arguments(arguments),
                ))
            }
            Self::Complete => return super::completion::request(value).ok_or_else(invalid),
        })
    }
}

pub(super) fn valid_arguments(arguments: &BTreeMap<String, String>) -> bool {
    arguments.len() <= 32
        && arguments
            .iter()
            .all(|(key, value)| valid_name(key) && value.len() <= 8192)
}
