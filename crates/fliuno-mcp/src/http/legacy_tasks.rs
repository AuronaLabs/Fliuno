//! Preserve old Tasks fields only on correlated initialize, discovery, and
//! task replies. The SDK still owns sessions, routing, and transport state.
use crate::{
    legacy_wire::{Negotiation, WireClient as LegacyWire},
    oauth::OAuthGrant,
};
use futures_util::StreamExt;
use reqwest::{
    Client, StatusCode, Url,
    header::{self, HeaderName, HeaderValue},
};
use rmcp::{
    RoleClient,
    model::{ClientJsonRpcMessage, JsonRpcMessage},
    service::{RxJsonRpcMessage, TxJsonRpcMessage},
    transport::{
        Transport,
        common::client_side_sse::BoxedSseResponse,
        streamable_http_client::{
            StreamableHttpClient, StreamableHttpError, StreamableHttpPostResponse,
            StreamableHttpResponseLimits,
        },
    },
};
use std::{collections::HashMap, future::Future, sync::Arc};

type HttpError = StreamableHttpError<reqwest::Error>;
type PostReply = Result<StreamableHttpPostResponse, HttpError>;
type Headers = HashMap<HeaderName, HeaderValue>;
const SESSION_ID: &str = "mcp-session-id";

#[derive(Clone)]
pub(super) struct WireClient {
    pub client: Client,
    pub grant: Option<Arc<OAuthGrant>>,
    pub negotiation: Option<Negotiation>,
}

impl WireClient {
    async fn token(
        &self,
        uri: &str,
        fallback: Option<String>,
    ) -> Result<Option<String>, HttpError> {
        let Some(grant) = &self.grant else {
            return Ok(fallback);
        };
        let url = Url::parse(uri).map_err(|_| invalid("invalid MCP OAuth request target"))?;
        let header = grant
            .authorization(&url)
            .await
            .map_err(|_| invalid("MCP OAuth authorization is unavailable"))?;
        let token = header
            .to_str()
            .ok()
            .and_then(|value| value.strip_prefix("Bearer "))
            .ok_or_else(|| invalid("invalid MCP OAuth bearer header"))?;
        Ok(Some(token.to_owned()))
    }

    fn redact<T>(&self, result: Result<T, HttpError>) -> Result<T, HttpError> {
        result.map_err(|error| match error {
            HttpError::SessionExpired
            | HttpError::ServerDoesNotSupportSse
            | HttpError::ServerDoesNotSupportDeleteSession => error,
            _ if self.grant.is_some() => invalid("MCP OAuth HTTP request failed"),
            _ => error,
        })
    }
}

impl StreamableHttpClient for WireClient {
    type Error = reqwest::Error;

    async fn post_message(
        &self,
        uri: Arc<str>,
        message: ClientJsonRpcMessage,
        session: Option<Arc<str>>,
        auth: Option<String>,
        headers: Headers,
    ) -> PostReply {
        self.post_message_with_response_limits(
            uri,
            message,
            session,
            auth,
            headers,
            Default::default(),
        )
        .await
    }

    async fn post_message_with_response_limits(
        &self,
        uri: Arc<str>,
        message: ClientJsonRpcMessage,
        session: Option<Arc<str>>,
        auth: Option<String>,
        headers: Headers,
        limits: StreamableHttpResponseLimits,
    ) -> PostReply {
        let auth = self.token(&uri, auth).await?;
        let Some(negotiation) = &self.negotiation else {
            return self.redact(
                self.client
                    .post_message_with_response_limits(uri, message, session, auth, headers, limits)
                    .await,
            );
        };
        let (message, tracked) = negotiation.prepare(message);
        let Some(expected) = tracked else {
            return self.redact(
                self.client
                    .post_message_with_response_limits(uri, message, session, auth, headers, limits)
                    .await,
            );
        };
        let mut post = self
            .client
            .post(uri.as_ref())
            .headers(headers.into_iter().collect())
            .header(header::ACCEPT, "application/json, text/event-stream");
        if let Some(session) = &session {
            post = post.header(SESSION_ID, session.as_ref());
        }
        if let Some(auth) = auth {
            post = post.bearer_auth(auth);
        }
        let response = self.redact(post.json(&message).send().await.map_err(HttpError::Client))?;
        let status = response.status();
        if matches!(
            status,
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN | StatusCode::TOO_MANY_REQUESTS
        ) {
            return Err(invalid("legacy MCP request was denied"));
        }
        if status == StatusCode::NOT_FOUND && session.is_some() {
            return Err(HttpError::SessionExpired);
        }
        let media = response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<mime::Mime>().ok());
        let returned_session = response
            .headers()
            .get(SESSION_ID)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        match media.as_ref().map(mime::Mime::essence_str) {
            Some("application/json") => {
                let body = bounded_json(response, limits.max_json_response_size).await?;
                let raw: RxJsonRpcMessage<LegacyWire> = serde_json::from_slice(&body)
                    .map_err(|_| invalid("invalid legacy MCP JSON envelope"))?;
                match &raw {
                    JsonRpcMessage::Response(reply)
                        if status.is_success() && reply.id == expected => {}
                    JsonRpcMessage::Error(error) if error.id.as_ref() == Some(&expected) => {}
                    _ => {
                        return Err(invalid(
                            "legacy MCP response does not match the POST request",
                        ));
                    }
                }
                let reply = negotiation
                    .receive(raw)
                    .ok_or_else(|| invalid("unsupported legacy MCP message"))?;
                Ok(StreamableHttpPostResponse::Json(reply, returned_session))
            }
            Some("text/event-stream") if status.is_success() => {
                let negotiation = negotiation.clone();
                let stream = super::legacy_sse::bounded_events(response, limits.max_sse_event_size)
                    .map(move |event| {
                        if let Ok(event) = &event
                            && let Some(data) = &event.data
                        {
                            negotiation.observe_http_event(data, &expected);
                        }
                        event
                    })
                    .boxed();
                Ok(StreamableHttpPostResponse::Sse(stream, returned_session))
            }
            _ => Err(invalid(
                "legacy MCP POST did not return a JSON or SSE reply",
            )),
        }
    }

    async fn delete_session(
        &self,
        uri: Arc<str>,
        session: Arc<str>,
        auth: Option<String>,
        headers: Headers,
    ) -> Result<(), HttpError> {
        let auth = self.token(&uri, auth).await?;
        self.redact(
            self.client
                .delete_session(uri, session, auth, headers)
                .await,
        )
    }

    async fn get_stream(
        &self,
        uri: Arc<str>,
        session: Option<Arc<str>>,
        last_event_id: Option<String>,
        auth: Option<String>,
        headers: Headers,
    ) -> Result<BoxedSseResponse, HttpError> {
        let auth = self.token(&uri, auth).await?;
        self.redact(
            self.client
                .get_stream(uri, session, last_event_id, auth, headers)
                .await,
        )
    }

    async fn get_stream_with_max_sse_event_size(
        &self,
        uri: Arc<str>,
        session: Option<Arc<str>>,
        last_event_id: Option<String>,
        auth: Option<String>,
        headers: Headers,
        limit: usize,
    ) -> Result<BoxedSseResponse, HttpError> {
        let auth = self.token(&uri, auth).await?;
        self.redact(
            self.client
                .get_stream_with_max_sse_event_size(
                    uri,
                    session,
                    last_event_id,
                    auth,
                    headers,
                    limit,
                )
                .await,
        )
    }
}

async fn bounded_json(mut response: reqwest::Response, limit: usize) -> Result<Vec<u8>, HttpError> {
    let too_large = || HttpError::ResponseBodyTooLarge { max_size: limit };
    if response
        .content_length()
        .is_some_and(|len| len > limit as u64)
    {
        return Err(too_large());
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(HttpError::Client)? {
        if chunk.len() > limit - body.len() {
            return Err(too_large());
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

fn invalid(message: &'static str) -> HttpError {
    HttpError::UnexpectedServerResponse(message.into())
}

pub(super) struct LegacyTransport<T> {
    pub inner: T,
    pub negotiation: Negotiation,
}

impl<T: Transport<RoleClient>> Transport<RoleClient> for LegacyTransport<T> {
    type Error = T::Error;

    fn send(
        &mut self,
        message: TxJsonRpcMessage<RoleClient>,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send + 'static {
        self.inner.send(message)
    }

    async fn receive(&mut self) -> Option<RxJsonRpcMessage<RoleClient>> {
        let mut message = self.inner.receive().await?;
        self.negotiation.complete_http_reply(&mut message);
        Some(message)
    }

    async fn close(&mut self) -> Result<(), Self::Error> {
        self.inner.close().await
    }
}
