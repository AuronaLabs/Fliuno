//! Bounded native model listing. Discovery never starts a generation request.
use crate::{
    http::{Http, provider_headers},
    provider_base,
};
use fliuno_config::{ModelSettings, Provider, ReasoningEffort};
use fliuno_protocol::{ProviderModel, ProviderModelPage};
use serde_json::Value;
use std::collections::BTreeSet;

const MAX_MODELS_PER_PAGE: usize = 256;

pub fn discover_models(
    settings: &ModelSettings,
    cursor: Option<&str>,
) -> Result<ProviderModelPage, String> {
    let provider = settings.provider;
    if cursor.is_some_and(|value| !bounded(value, 2048)) {
        return Err("Invalid model catalog cursor".into());
    }
    let mut params = match provider {
        Provider::Demo => return Err("Choose a configured live model provider".into()),
        Provider::OpenAi => {
            if cursor.is_some() {
                return Err("OpenAI model lists do not accept a cursor".into());
            }
            vec![]
        }
        Provider::Anthropic => vec![("limit", "100")],
        Provider::Gemini => vec![("pageSize", "100")],
    };
    if let Some(cursor) = cursor {
        params.push((
            if matches!(provider, Provider::Anthropic) {
                "after_id"
            } else {
                "pageToken"
            },
            cursor,
        ));
    }
    let key = settings.read_api_key()?;
    let headers = provider_headers(provider, &key)?;
    let http = Http::new(
        provider_base(provider, settings.base_url.as_deref()),
        "models",
        "models",
        &[],
        headers,
    )?;
    let value = http.model_catalog(&params)?;
    let mut next = match provider {
        Provider::Anthropic if value["has_more"] == true => Some(
            value["last_id"]
                .as_str()
                .ok_or("Provider omitted its next model cursor")?,
        ),
        Provider::Gemini => value["nextPageToken"]
            .as_str()
            .filter(|token| !token.is_empty()),
        _ => None,
    };
    if next.is_some_and(|value| !bounded(value, 2048) || Some(value) == cursor) {
        return Err("Provider returned an invalid model cursor".into());
    }
    let entries = value[if matches!(provider, Provider::Gemini) {
        "models"
    } else {
        "data"
    }]
    .as_array()
    .ok_or("Provider did not return a model list")?;
    let mut models = Vec::new();
    let mut seen = BTreeSet::<&str>::new();
    let mut truncated = false;
    for entry in entries {
        let (id, label) = match provider {
            Provider::OpenAi => (entry["id"].as_str(), None),
            Provider::Anthropic => {
                if entry["lifecycle"] == "retired" {
                    continue;
                }
                (entry["id"].as_str(), entry["display_name"].as_str())
            }
            Provider::Gemini => {
                let generates_text =
                    entry["supportedGenerationMethods"]
                        .as_array()
                        .is_some_and(|methods| {
                            methods.iter().any(|method| method == "generateContent")
                        });
                if !generates_text {
                    continue;
                }
                (
                    entry["name"]
                        .as_str()
                        .and_then(|name| name.strip_prefix("models/")),
                    entry["displayName"].as_str(),
                )
            }
            Provider::Demo => unreachable!(),
        };
        let Some(id) = id.filter(|id| bounded(id, 256) && seen.insert(*id)) else {
            continue;
        };
        if models.len() == MAX_MODELS_PER_PAGE {
            truncated = true;
            // A native cursor advances beyond the overflowing page.
            next = None;
            break;
        }
        let (input, output, thinking, efforts) = match provider {
            Provider::Anthropic => (
                token_limit(&entry["max_input_tokens"]),
                token_limit(&entry["max_tokens"]),
                entry["capabilities"]["thinking"]["supported"].as_bool(),
                anthropic_efforts(&entry["capabilities"]["effort"]),
            ),
            Provider::Gemini => (
                token_limit(&entry["inputTokenLimit"]),
                token_limit(&entry["outputTokenLimit"]),
                entry["thinking"].as_bool(),
                None,
            ),
            _ => (None, None, None, None),
        };
        models.push(ProviderModel {
            id: id.to_owned(),
            display_name: label.filter(|name| bounded(name, 256)).map(str::to_owned),
            max_input_tokens: input,
            max_output_tokens: output,
            supports_thinking: thinking,
            reasoning_efforts: efforts,
        });
    }
    Ok(ProviderModelPage {
        provider: provider.name().into(),
        models,
        next_cursor: next.map(str::to_owned),
        truncated,
    })
}

fn token_limit(value: &Value) -> Option<u32> {
    u32::try_from(value.as_u64()?)
        .ok()
        .filter(|limit| *limit > 0)
}

fn anthropic_efforts(capability: &Value) -> Option<Vec<String>> {
    if capability["supported"].as_bool() == Some(false) {
        return Some(Vec::new());
    }
    let mut reported = [
        ReasoningEffort::Low,
        ReasoningEffort::Medium,
        ReasoningEffort::High,
        ReasoningEffort::XHigh,
        ReasoningEffort::Max,
    ]
    .into_iter()
    .filter_map(|effort| {
        let name = effort.as_str();
        Some((name, capability[name]["supported"].as_bool()?))
    })
    .peekable();
    reported.peek()?;
    Some(
        reported
            .filter(|(_, supported)| *supported)
            .map(|(name, _)| name.to_owned())
            .collect(),
    )
}

fn bounded(value: &str, max: usize) -> bool {
    !value.trim().is_empty() && value.len() <= max && !value.chars().any(char::is_control)
}
