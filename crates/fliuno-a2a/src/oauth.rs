use crate::{Error, Result, client::bearer_header, validate_endpoint};
use reqwest::{Url, header::HeaderValue};
use serde::Deserialize;
use std::time::Duration;
use tokio::{sync::Mutex, time::Instant};

/// Host-resolved OAuth configuration. Debug is intentionally not derived.
#[derive(Clone)]
pub struct ClientCredentials {
    pub token_endpoint: String,
    pub client_id: String,
    pub client_secret: String,
    pub scope: Option<String>,
}

pub(crate) struct OAuth {
    endpoint: Url,
    options: ClientCredentials,
    http: reqwest::Client,
    cached: Mutex<Option<CachedToken>>,
}

struct CachedToken {
    bearer: HeaderValue,
    refresh_after: Instant,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    token_type: String,
    expires_in: u64,
}

impl OAuth {
    pub(crate) fn new(options: ClientCredentials, timeout: Duration) -> Result<Self> {
        let endpoint = validate_endpoint(&options.token_endpoint)?;
        let valid = |value: &str, max: usize| {
            !value.trim().is_empty() && value.len() <= max && !value.chars().any(char::is_control)
        };
        if !valid(&options.client_id, 512)
            || !valid(&options.client_secret, 8192)
            || options
                .scope
                .as_ref()
                .is_some_and(|value| !valid(value, 2048))
        {
            return Err(Error::Config);
        }
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .no_proxy()
            .timeout(timeout)
            .build()
            .map_err(|_| Error::Config)?;
        Ok(Self {
            endpoint,
            options,
            http,
            cached: Mutex::new(None),
        })
    }

    pub(crate) async fn bearer(&self) -> Result<HeaderValue> {
        let mut cached = self.cached.lock().await;
        if let Some(token) = cached
            .as_ref()
            .filter(|token| Instant::now() < token.refresh_after)
        {
            return Ok(token.bearer.clone());
        }
        let started = Instant::now();
        let mut fields = vec![
            ("grant_type", "client_credentials"),
            ("client_id", self.options.client_id.as_str()),
            ("client_secret", self.options.client_secret.as_str()),
        ];
        if let Some(scope) = &self.options.scope {
            fields.push(("scope", scope));
        }
        let response = self
            .http
            .post(self.endpoint.clone())
            .header(reqwest::header::ACCEPT, "application/json")
            .form(&fields)
            .send()
            .await
            .map_err(|_| Error::Authentication)?;
        let token: TokenResponse = crate::client::read_json(response, 32 * 1024)
            .await
            .map_err(|_| Error::Authentication)?;
        if !token.token_type.eq_ignore_ascii_case("Bearer") || token.expires_in == 0 {
            return Err(Error::Authentication);
        }
        let bearer = bearer_header(&token.access_token).map_err(|_| Error::Authentication)?;
        let lifetime = Duration::from_secs(token.expires_in.min(86_400));
        let expires = started + lifetime;
        if Instant::now() >= expires {
            return Err(Error::Authentication);
        }
        let refresh_after = expires - (lifetime / 5).min(Duration::from_secs(30));
        *cached = Some(CachedToken {
            bearer: bearer.clone(),
            refresh_after,
        });
        Ok(bearer)
    }
}
