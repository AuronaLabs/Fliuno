//! Bounded native model listing. Discovery never starts a generation request.
use crate::{
    http::{Http, MetadataError, provider_headers},
    provider_base,
};
use fliuno_config::{
    ModelCatalogDraft, ModelSettings, Provider, ReasoningEffort, provider_api_root,
    validate_api_key,
};
use fliuno_protocol::{
    ModelCatalogAccess, ModelCatalogPreview, ModelCatalogStatus, ModelConfigurationStatus,
    ModelConnectionCheck, ModelCredentialsStatus, ModelPresence, ModelReasoningCheck,
    ModelReasoningEffortStatus, ProviderModel, ProviderModelPage,
};
use serde_json::Value;
use std::{
    collections::BTreeSet,
    time::{Duration, Instant},
};

const MAX_MODELS_PER_PAGE: usize = 256;
const MAX_CHECK_PAGES: u8 = 4;
const CATALOG_TIMEOUT: Duration = Duration::from_secs(5);

pub fn discover_models(
    settings: &ModelSettings,
    cursor: Option<&str>,
) -> Result<ProviderModelPage, String> {
    settings.effective_protocol()?;
    let provider = settings.provider;
    if matches!(provider, Provider::Demo) {
        return Err("Choose a configured live model provider".into());
    }
    let headers = provider_headers(provider, &settings.read_api_key()?)?;
    let http = catalog_http(settings, headers)?;
    read_page(provider, &http, cursor, CATALOG_TIMEOUT).map_err(|error| error.to_string())
}

/// Preview a draft's catalog without saving settings or credentials.
pub fn preview_model_catalog(
    draft: ModelCatalogDraft,
    key: &str,
) -> Result<ModelCatalogPreview, String> {
    let settings = ModelSettings {
        provider: draft.provider,
        protocol: Some(draft.protocol),
        base_url: draft.base_url,
        ..Default::default()
    };
    validate_configuration(&settings)?;
    validate_api_key(key)?;
    let headers = provider_headers(settings.provider, key)?;
    let http = catalog_http(&settings, headers)?;
    Ok(
        match read_page(
            settings.provider,
            &http,
            draft.cursor.as_deref(),
            CATALOG_TIMEOUT,
        ) {
            Ok(mut page) => {
                // An untrusted catalog can echo the supplied credential in JSON fields.
                let echoes_key =
                    |value: &str| value == key || (key.len() >= 8 && value.contains(key));
                page.models.retain(|entry| !echoes_key(&entry.id));
                for entry in &mut page.models {
                    if entry.display_name.as_deref().is_some_and(echoes_key) {
                        entry.display_name = None;
                    }
                }
                if page.next_cursor.as_deref().is_some_and(echoes_key) {
                    page.next_cursor = None;
                    page.truncated = true;
                }
                ModelCatalogPreview {
                    catalog: ModelCatalogStatus::Available,
                    catalog_access: ModelCatalogAccess::Accepted,
                    http_status: None,
                    page: Some(page),
                }
            }
            Err(error @ CatalogError::Input(_)) => return Err(error.to_string()),
            Err(error) => {
                let (catalog, catalog_access, http_status) = classify_failure(error);
                ModelCatalogPreview {
                    catalog,
                    catalog_access,
                    http_status,
                    page: None,
                }
            }
        },
    )
}

/// Check a trusted local profile without generating or resolving a remote context budget.
pub fn check_model_connection(settings: &ModelSettings) -> ModelConnectionCheck {
    let model = settings
        .name
        .as_deref()
        .filter(|name| !name.trim().is_empty());
    let mut check = ModelConnectionCheck {
        configuration: ModelConfigurationStatus::Ready,
        configuration_issue: None,
        credentials: ModelCredentialsStatus::NotChecked,
        catalog_access: ModelCatalogAccess::Unverified,
        catalog: ModelCatalogStatus::NotChecked,
        model_presence: if model.is_some() {
            ModelPresence::NotChecked
        } else {
            ModelPresence::NotSelected
        },
        http_status: None,
        pages_checked: 0,
        reasoning: reasoning_check(settings.reasoning_effort, None),
    };
    if let Err(issue) = validate_configuration(settings) {
        check.configuration = ModelConfigurationStatus::Invalid;
        check.configuration_issue = Some(issue);
        return check;
    }
    if model.is_none() {
        check.configuration = ModelConfigurationStatus::MissingModel;
    }
    let provider = settings.provider;
    let headers = settings
        .read_api_key()
        .map_err(str::to_owned)
        .and_then(|key| provider_headers(provider, &key));
    let Ok(headers) = headers else {
        check.credentials = ModelCredentialsStatus::Unavailable;
        return check;
    };
    check.credentials = ModelCredentialsStatus::Ready;
    let Ok(http) = catalog_http(settings, headers) else {
        check.catalog = ModelCatalogStatus::TransportError;
        return check;
    };
    let deadline = Instant::now() + CATALOG_TIMEOUT;
    let mut cursors = BTreeSet::new();
    let mut cursor = None;
    for _ in 0..MAX_CHECK_PAGES {
        let timeout = deadline.saturating_duration_since(Instant::now());
        if timeout.is_zero() {
            record_failure(&mut check, CatalogError::Request(MetadataError::Timeout));
            break;
        }
        let page = match read_page(provider, &http, cursor.as_deref(), timeout) {
            Ok(page) => page,
            Err(error) => {
                record_failure(&mut check, error);
                break;
            }
        };
        check.pages_checked += 1;
        check.catalog = ModelCatalogStatus::Available;
        check.catalog_access = ModelCatalogAccess::Accepted;
        let Some(model) = model else { break };
        if let Some(entry) = page.models.into_iter().find(|entry| entry.id == model) {
            check.model_presence = ModelPresence::Present;
            check.reasoning = reasoning_check(settings.reasoning_effort, Some(entry));
            break;
        }
        check.model_presence = ModelPresence::Incomplete;
        if page.truncated {
            break;
        }
        let Some(next) = page.next_cursor else {
            check.model_presence = ModelPresence::NotListed;
            break;
        };
        if !cursors.insert(next.clone()) {
            record_failure(
                &mut check,
                CatalogError::Document("Provider repeated a model cursor"),
            );
            break;
        }
        cursor = Some(next);
    }
    check
}

fn reasoning_check(
    configured: Option<ReasoningEffort>,
    entry: Option<ProviderModel>,
) -> Option<ModelReasoningCheck> {
    let supports_thinking = entry.as_ref().and_then(|entry| entry.supports_thinking);
    let advertised_efforts = entry.and_then(|entry| entry.reasoning_efforts);
    if configured.is_none() && supports_thinking.is_none() && advertised_efforts.is_none() {
        return None;
    }
    let effort_status = match (configured, &advertised_efforts) {
        (None, _) => ModelReasoningEffortStatus::ProviderDefault,
        (Some(_), None) => ModelReasoningEffortStatus::Unknown,
        (Some(effort), Some(levels)) if levels.iter().any(|level| level == effort.as_str()) => {
            ModelReasoningEffortStatus::Advertised
        }
        (Some(_), Some(_)) => ModelReasoningEffortStatus::NotAdvertised,
    };
    Some(ModelReasoningCheck {
        configured_effort: configured.map(|effort| effort.as_str().to_owned()),
        supports_thinking,
        advertised_efforts,
        effort_status,
    })
}

fn validate_configuration(settings: &ModelSettings) -> Result<(), String> {
    if settings.effective_protocol()?.is_none() {
        return Err("Choose a configured live model provider".into());
    }
    settings.validate_reasoning_effort()?;
    settings.validate_budget()?;
    provider_api_root(provider_base(
        settings.provider,
        settings.base_url.as_deref(),
    ))?;
    Ok(())
}

fn record_failure(check: &mut ModelConnectionCheck, error: CatalogError) {
    let (status, access, http_status) = classify_failure(error);
    check.catalog = status;
    check.http_status = http_status;
    if access != ModelCatalogAccess::Unverified {
        check.catalog_access = access;
    }
}

fn classify_failure(error: CatalogError) -> (ModelCatalogStatus, ModelCatalogAccess, Option<u16>) {
    use ModelCatalogStatus::*;
    let status = match error {
        CatalogError::Request(MetadataError::Http(status)) => {
            let access = match status {
                401 => ModelCatalogAccess::Unauthorized,
                403 => ModelCatalogAccess::Forbidden,
                _ => ModelCatalogAccess::Unverified,
            };
            let catalog = if matches!(status, 404 | 405 | 501) {
                Unavailable
            } else {
                HttpError
            };
            return (catalog, access, Some(status));
        }
        CatalogError::Request(MetadataError::Timeout) => TimedOut,
        CatalogError::Request(MetadataError::Transport) => TransportError,
        _ => InvalidResponse,
    };
    (status, ModelCatalogAccess::Unverified, None)
}

fn catalog_http(
    settings: &ModelSettings,
    headers: reqwest::header::HeaderMap,
) -> Result<Http, String> {
    Http::new(
        provider_base(settings.provider, settings.base_url.as_deref()),
        "models",
        "models",
        &[],
        headers,
    )
}

#[derive(Debug)]
enum CatalogError {
    Input(&'static str),
    Document(&'static str),
    Request(MetadataError),
}

impl std::fmt::Display for CatalogError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Input(message) | Self::Document(message) => formatter.write_str(message),
            Self::Request(error) => error.fmt(formatter),
        }
    }
}

fn read_page(
    provider: Provider,
    http: &Http,
    cursor: Option<&str>,
    timeout: Duration,
) -> Result<ProviderModelPage, CatalogError> {
    if cursor.is_some_and(|value| !bounded(value, 2048)) {
        return Err(CatalogError::Input("Invalid model catalog cursor"));
    }
    let mut params = match provider {
        Provider::Demo => {
            return Err(CatalogError::Input(
                "Choose a configured live model provider",
            ));
        }
        Provider::OpenAi => {
            if cursor.is_some() {
                return Err(CatalogError::Input(
                    "OpenAI model lists do not accept a cursor",
                ));
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
    let value = http
        .model_catalog(&params, timeout)
        .map_err(CatalogError::Request)?;
    let mut next = match provider {
        Provider::Anthropic if value["has_more"] == true => Some(value["last_id"].as_str().ok_or(
            CatalogError::Document("Provider omitted its next model cursor"),
        )?),
        Provider::Gemini => value["nextPageToken"]
            .as_str()
            .filter(|token| !token.is_empty()),
        _ => None,
    };
    if next.is_some_and(|value| !bounded(value, 2048) || Some(value) == cursor) {
        return Err(CatalogError::Document(
            "Provider returned an invalid model cursor",
        ));
    }
    let entries = value[if matches!(provider, Provider::Gemini) {
        "models"
    } else {
        "data"
    }]
    .as_array()
    .ok_or(CatalogError::Document(
        "Provider did not return a model list",
    ))?;
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
