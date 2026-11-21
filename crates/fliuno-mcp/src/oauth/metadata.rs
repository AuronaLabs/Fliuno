use super::{OAuthClient, OAuthEndpointConfig, OAuthError, endpoint};
use reqwest::{StatusCode, Url};
use serde::Deserialize;

const RESOURCE_METADATA: &str = "/.well-known/oauth-protected-resource";

#[derive(Deserialize)]
struct ResourceMetadata {
    resource: String,
    authorization_servers: Vec<String>,
    #[serde(default)]
    scopes_supported: Vec<String>,
}

#[derive(Deserialize)]
struct ServerMetadata {
    issuer: String,
    authorization_endpoint: String,
    token_endpoint: String,
    registration_endpoint: Option<String>,
    code_challenge_methods_supported: Vec<String>,
    response_types_supported: Vec<String>,
    grant_types_supported: Option<Vec<String>>,
    token_endpoint_auth_methods_supported: Option<Vec<String>>,
    #[serde(default)]
    client_id_metadata_document_supported: bool,
    #[serde(default)]
    authorization_response_iss_parameter_supported: bool,
}

pub(super) struct Discovery {
    pub authorize: Url,
    pub token: Url,
    pub registration: Option<Url>,
    pub supports_cimd: bool,
    pub requires_callback_issuer: bool,
    pub scopes: Vec<String>,
}

impl OAuthClient {
    pub(super) async fn discover(&self, challenge: Option<&str>) -> Result<Discovery, OAuthError> {
        let (advertised, challenge_scope) = parse_challenge(challenge)?;
        let resource = if let Some(advertised) = advertised {
            let advertised = endpoint(&advertised, self.resource.scheme() == "http")?;
            if advertised.origin() != self.resource.origin() {
                return Err(OAuthError::UnexpectedEndpoint);
            }
            self.get_json::<ResourceMetadata>(&advertised).await?
        } else {
            let mut urls = vec![well_known(&self.resource, RESOURCE_METADATA)];
            let root = with_path(&self.resource, RESOURCE_METADATA);
            if urls[0] != root {
                urls.push(root);
            }
            self.first_metadata::<ResourceMetadata>(urls).await?
        }
        .ok_or(OAuthError::MissingMetadata)?;
        if endpoint(&resource.resource, self.resource.scheme() == "http")? != self.resource
            || resource.authorization_servers.len() > 16
            || !resource
                .authorization_servers
                .iter()
                .any(|issuer| issuer == &self.issuer_id)
        {
            return Err(OAuthError::IssuerMismatch);
        }
        let issuer_path = self.issuer.path().trim_end_matches('/');
        let mut urls = vec![
            well_known(&self.issuer, "/.well-known/oauth-authorization-server"),
            well_known(&self.issuer, "/.well-known/openid-configuration"),
        ];
        if !issuer_path.is_empty() {
            urls.push(with_path(
                &self.issuer,
                &format!("{issuer_path}/.well-known/openid-configuration"),
            ));
        }
        let auth = self
            .first_metadata::<ServerMetadata>(urls)
            .await?
            .ok_or(OAuthError::MissingMetadata)?;
        if auth.issuer != self.issuer_id {
            return Err(OAuthError::IssuerMismatch);
        }
        if !auth
            .code_challenge_methods_supported
            .iter()
            .any(|mode| mode == "S256")
            || !auth
                .response_types_supported
                .iter()
                .any(|mode| mode == "code")
            || auth
                .grant_types_supported
                .as_ref()
                .is_some_and(|grants| !grants.iter().any(|grant| grant == "authorization_code"))
            || auth
                .token_endpoint_auth_methods_supported
                .as_ref()
                .is_some_and(|methods| !methods.iter().any(|method| method == "none"))
        {
            return Err(OAuthError::UnsupportedFlow);
        }
        let scopes = if self.scopes.is_empty() {
            challenge_scope.unwrap_or(resource.scopes_supported)
        } else {
            self.scopes.clone()
        };
        validate_scopes(&scopes)?;
        let authorize = self.authorization_endpoint(&auth.authorization_endpoint)?;
        let token = self.authorization_endpoint(&auth.token_endpoint)?;
        let registration =
            if !auth.client_id_metadata_document_supported && self.allow_dynamic_registration {
                auth.registration_endpoint
                    .as_deref()
                    .map(|url| self.authorization_endpoint(url))
                    .transpose()?
            } else {
                None
            };
        Ok(Discovery {
            authorize,
            token,
            registration,
            supports_cimd: auth.client_id_metadata_document_supported,
            requires_callback_issuer: auth.authorization_response_iss_parameter_supported,
            scopes,
        })
    }

    async fn first_metadata<T: serde::de::DeserializeOwned>(
        &self,
        urls: Vec<Url>,
    ) -> Result<Option<T>, OAuthError> {
        for url in urls {
            if let Some(metadata) = self.get_json(&url).await? {
                return Ok(Some(metadata));
            }
        }
        Ok(None)
    }

    pub(super) async fn get_json<T: serde::de::DeserializeOwned>(
        &self,
        url: &Url,
    ) -> Result<Option<T>, OAuthError> {
        let response = self
            .http
            .get(url.clone())
            .header(reqwest::header::ACCEPT, "application/json")
            .send()
            .await
            .map_err(|_| OAuthError::Transport)?;
        match response.status() {
            StatusCode::NOT_FOUND | StatusCode::METHOD_NOT_ALLOWED => Ok(None),
            StatusCode::OK => super::read_json(response).await.map(Some),
            _ => Err(OAuthError::MetadataRejected),
        }
    }
}

fn well_known(base: &Url, prefix: &str) -> Url {
    with_path(
        base,
        &format!("{prefix}{}", base.path().trim_end_matches('/')),
    )
}

fn with_path(base: &Url, path: &str) -> Url {
    let mut url = base.clone();
    url.set_path(path);
    url.set_query(None);
    url
}

fn parse_challenge(
    challenge: Option<&str>,
) -> Result<(Option<String>, Option<Vec<String>>), OAuthError> {
    let Some(challenge) = challenge else {
        return Ok((None, None));
    };
    if challenge.len() > 8192 {
        return Err(OAuthError::MetadataRejected);
    }
    let mut url = None;
    let mut scopes = None;
    let challenges =
        http_auth::parse_challenges(challenge).map_err(|_| OAuthError::MetadataRejected)?;
    for challenge in challenges
        .iter()
        .filter(|challenge| challenge.scheme.eq_ignore_ascii_case("Bearer"))
    {
        for (name, value) in &challenge.params {
            if name.eq_ignore_ascii_case("resource_metadata") {
                if url.replace(value.to_unescaped()).is_some() {
                    return Err(OAuthError::MetadataRejected);
                }
            } else if name.eq_ignore_ascii_case("scope")
                && scopes
                    .replace(
                        value
                            .to_unescaped()
                            .split_ascii_whitespace()
                            .map(str::to_owned)
                            .collect(),
                    )
                    .is_some()
            {
                return Err(OAuthError::MetadataRejected);
            }
        }
    }
    Ok((url, scopes))
}

pub(super) fn validate_scopes(scopes: &[String]) -> Result<(), OAuthError> {
    if scopes.len() > 32
        || scopes.iter().any(|scope| {
            scope.is_empty()
                || scope.len() > 128
                || !scope
                    .bytes()
                    .all(|byte| matches!(byte, 0x21 | 0x23..=0x5b | 0x5d..=0x7e))
        })
    {
        return Err(OAuthError::UnsupportedFlow);
    }
    Ok(())
}

pub(super) fn validate_config(config: &OAuthEndpointConfig) -> Result<(), OAuthError> {
    validate_scopes(&config.scopes)?;
    if config.authorization_endpoint_origins.len() > 8 {
        return Err(OAuthError::UnexpectedEndpoint);
    }
    Ok(())
}
