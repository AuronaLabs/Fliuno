use super::*;
use axum::{
    Router,
    body::Bytes,
    extract::State,
    http::{HeaderMap, Method, StatusCode, Uri, header},
    response::{IntoResponse, Response},
};
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};

struct Fixture {
    url: String,
    cimd: bool,
    wrong_issuer: bool,
    fail_refresh: bool,
    hold_refresh: AtomicBool,
    registrations: AtomicUsize,
    client_documents: AtomicUsize,
    exchanges: AtomicUsize,
    refreshes: AtomicUsize,
    calls: AtomicUsize,
    challenge: Mutex<String>,
}

async fn route(
    State(fixture): State<Arc<Fixture>>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> impl IntoResponse {
    if uri.path() == "/mcp" {
        assert_eq!(headers[header::AUTHORIZATION], "Bearer refreshed-access");
        return mcp_reply(method, &body, &fixture);
    }
    let root = &fixture.url;
    let issuer = format!("{root}/tenant");
    let resource = format!("{root}/mcp");
    let (status, reply) = match (method, uri.path()) {
        (Method::GET, "/meta" | "/.well-known/oauth-protected-resource") => (
            StatusCode::OK,
            json!({
                "resource":resource,"authorization_servers":[issuer],"scopes_supported":["tools"]
            }),
        ),
        (Method::GET, "/.well-known/openid-configuration/tenant") => (
            StatusCode::OK,
            json!({
                "issuer":if fixture.wrong_issuer {"https://wrong.example"} else {&issuer},
                "authorization_endpoint":format!("{root}/authorize"),"token_endpoint":format!("{root}/token"),
                "registration_endpoint":format!("{root}/register"),"response_types_supported":["code"],
                "code_challenge_methods_supported":["S256"],"token_endpoint_auth_methods_supported":["none"],
                "authorization_response_iss_parameter_supported":true,"client_id_metadata_document_supported":fixture.cimd
            }),
        ),
        (Method::GET, "/client.json") => {
            fixture.client_documents.fetch_add(1, Ordering::SeqCst);
            (
                StatusCode::OK,
                json!({"client_id":format!("{root}/client.json"),"client_name":"Fixture client",
                    "redirect_uris":["http://127.0.0.1:8022/callback"],"token_endpoint_auth_method":"none"}),
            )
        }
        (Method::POST, "/register") => {
            fixture.registrations.fetch_add(1, Ordering::SeqCst);
            let request: serde_json::Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(request["token_endpoint_auth_method"], "none");
            (
                StatusCode::CREATED,
                json!({"client_id":"dcr-client","token_endpoint_auth_method":"none","redirect_uris":request["redirect_uris"]}),
            )
        }
        (Method::POST, "/token") => {
            let query = Url::parse(&format!(
                "https://example.test/?{}",
                std::str::from_utf8(&body).unwrap()
            ))
            .unwrap();
            let fields: BTreeMap<_, _> = query.query_pairs().collect();
            assert_eq!(fields["resource"], resource);
            assert_eq!(fields["client_id"], fixture.client_id());
            if fields["grant_type"] == "authorization_code" {
                fixture.exchanges.fetch_add(1, Ordering::SeqCst);
                assert_eq!(fields["code"], "fixture-code");
                assert_eq!(fields["redirect_uri"], "http://127.0.0.1:8022/callback");
                assert_eq!(
                    URL_SAFE_NO_PAD.encode(Sha256::digest(fields["code_verifier"].as_bytes())),
                    *fixture.challenge.lock().await
                );
                (
                    StatusCode::OK,
                    json!({"token_type":"Bearer","access_token":"initial-access","refresh_token":"single-use-refresh","expires_in":0}),
                )
            } else {
                fixture.refreshes.fetch_add(1, Ordering::SeqCst);
                assert_eq!(fields["grant_type"], "refresh_token");
                assert_eq!(fields["refresh_token"], "single-use-refresh");
                if fixture.hold_refresh.load(Ordering::SeqCst) {
                    std::future::pending::<()>().await;
                }
                if fixture.fail_refresh {
                    (
                        StatusCode::BAD_REQUEST,
                        json!({"error":"invalid_grant","error_description":"must-not-leak-token-value"}),
                    )
                } else {
                    (
                        StatusCode::OK,
                        json!({"token_type":"Bearer","access_token":"refreshed-access","refresh_token":"new-refresh","expires_in":3600}),
                    )
                }
            }
        }
        _ => (StatusCode::NOT_FOUND, json!({})),
    };
    json_reply(status, reply)
}

impl Fixture {
    fn config_at(url: &str) -> OAuthEndpointConfig {
        OAuthEndpointConfig {
            issuer: format!("{url}/tenant"),
            redirect_uri: "http://127.0.0.1:8022/callback".into(),
            client_id: None,
            client_metadata_url: None,
            allow_dynamic_registration: true,
            authorization_endpoint_origins: vec![],
            scopes: vec![],
        }
    }

    fn client_id(&self) -> String {
        if self.cimd {
            format!("{}/client.json", self.url)
        } else {
            "dcr-client".into()
        }
    }
}

fn json_reply(status: StatusCode, reply: serde_json::Value) -> Response {
    (
        status,
        [(header::CONTENT_TYPE, "application/json")],
        reply.to_string(),
    )
        .into_response()
}

fn mcp_reply(method: Method, body: &[u8], fixture: &Fixture) -> Response {
    match method {
        Method::GET => return StatusCode::METHOD_NOT_ALLOWED.into_response(),
        Method::DELETE => return StatusCode::NO_CONTENT.into_response(),
        Method::POST => {}
        _ => panic!("unexpected MCP HTTP verb"),
    }
    let request: serde_json::Value = serde_json::from_slice(body).unwrap();
    let result = match request["method"].as_str().unwrap() {
        "server/discover" => json!({"resultType":"complete","supportedVersions":["2026-07-28"],
            "capabilities":{"tools":{}},"ttlMs":0,"cacheScope":"private"}),
        "initialize" => json!({"protocolVersion":"2025-11-25","capabilities":{"tools":{}},
            "serverInfo":{"name":"oauth-fixture","version":"1"}}),
        "notifications/initialized" => return StatusCode::ACCEPTED.into_response(),
        "tools/list" => json!({"tools":[{"name":"echo","inputSchema":{"type":"object"}}]}),
        "tools/call" => {
            fixture.calls.fetch_add(1, Ordering::SeqCst);
            return (
                StatusCode::UNAUTHORIZED,
                [(
                    header::WWW_AUTHENTICATE,
                    "Bearer error=\"invalid_token\", error_description=\"private-fixture-text\"",
                )],
                "private-fixture-text",
            )
                .into_response();
        }
        other => panic!("unexpected MCP method: {other}"),
    };
    json_reply(
        StatusCode::OK,
        json!({"jsonrpc":"2.0","id":request["id"],"result":result}),
    )
}

async fn fixture(
    cimd: bool,
    wrong_issuer: bool,
    fail_refresh: bool,
) -> (Arc<Fixture>, OAuthClient, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let config = Fixture::config_at(&url);
    let client = OAuthClient::build(
        &format!("{url}/mcp"),
        &config,
        Duration::from_secs(3),
        None,
        true,
    )
    .unwrap();
    let fixture = Arc::new(Fixture {
        url,
        cimd,
        wrong_issuer,
        fail_refresh,
        hold_refresh: AtomicBool::new(false),
        registrations: AtomicUsize::new(0),
        client_documents: AtomicUsize::new(0),
        exchanges: AtomicUsize::new(0),
        refreshes: AtomicUsize::new(0),
        calls: AtomicUsize::new(0),
        challenge: Mutex::new(String::new()),
    });
    let app = Router::new().fallback(route).with_state(fixture.clone());
    let worker = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (fixture, client, worker)
}

async fn complete(fixture: &Fixture, client: &OAuthClient) -> OAuthGrant {
    let challenge = format!(
        "Basic realm=\"fixture\", Bearer resource_metadata=\"{}/meta\", scope=\"read write\"",
        fixture.url
    );
    let attempt = client.begin(Some(&challenge)).await.unwrap();
    let query: BTreeMap<_, _> = attempt.authorization_url().query_pairs().collect();
    assert_eq!(query["resource"], client.resource.as_str());
    assert_eq!(query["scope"], "read write");
    assert_eq!(query["client_id"], fixture.client_id());
    assert_eq!(query["code_challenge_method"], "S256");
    *fixture.challenge.lock().await = query["code_challenge"].to_string();
    let mut callback = client.redirect.clone();
    callback.query_pairs_mut().extend_pairs([
        ("state", query["state"].as_ref()),
        ("code", "fixture-code"),
        ("iss", client.issuer_id.as_str()),
    ]);
    attempt.finish(callback.as_str()).await.unwrap()
}

#[tokio::test]
async fn advertised_cimd_binds_the_metadata_url_to_authorization_exchange_and_refresh() {
    let (peer, mut client, worker) = fixture(true, false, false).await;
    // Only the in-process fixture bypasses the HTTPS requirement for this URL.
    let document = Url::parse(&peer.client_id()).unwrap();
    assert!(matches!(
        endpoint(document.as_str(), false),
        Err(OAuthError::UnexpectedEndpoint)
    ));
    client.client_metadata_url = Some(document);
    let grant = complete(&peer, &client).await;
    grant.authorization(&client.resource).await.unwrap();
    assert_eq!(peer.client_documents.load(Ordering::SeqCst), 1);
    assert_eq!(peer.exchanges.load(Ordering::SeqCst), 1);
    assert_eq!(peer.refreshes.load(Ordering::SeqCst), 1);
    assert_eq!(peer.registrations.load(Ordering::SeqCst), 0);
    worker.abort();
}

#[tokio::test]
async fn http_generations_attach_the_grant_and_never_replay_a_rejected_call() {
    for lifecycle in ["discover", "legacy"] {
        let (fixture, client, worker) = fixture(false, false, false).await;
        let grant = Arc::new(complete(&fixture, &client).await);
        let url = client.resource.clone();
        tokio::task::spawn_blocking(move || {
            let config = |url: &Url| -> crate::HttpServerConfig {
                serde_json::from_value(json!({"id":"oauth","transport":"streamable_http",
                    "url":url.as_str(),"lifecycle":lifecycle,"timeoutMs":3000}))
                .unwrap()
            };
            let other = url.join("other").unwrap();
            let error = crate::connect_oauth(config(&other), grant.clone())
                .err()
                .unwrap();
            assert!(error.contains("outside the configured policy"));
            let tools = crate::connect_oauth(config(&url), grant).unwrap();
            let call = crate::ToolCall {
                id: "call".into(),
                name: tools[0].spec().name,
                arguments: json!({}),
            };
            let context = crate::ToolContext {
                session_id: "session",
                task_id: "task",
                workspace: &fliuno_protocol::Workspace {
                    id: "workspace".into(),
                    root: "unused".into(),
                },
            };
            let error = tools[0]
                .execute(&context, &call, &crate::CancellationToken::new())
                .err()
                .unwrap();
            assert!(matches!(error, crate::ToolError::OutcomeUnknown(_)));
            assert!(!error.to_string().contains("private-fixture-text"));
        })
        .await
        .unwrap();
        assert_eq!(
            fixture.calls.load(Ordering::SeqCst),
            1,
            "lifecycle: {lifecycle}"
        );
        assert_eq!(fixture.refreshes.load(Ordering::SeqCst), 1);
        worker.abort();
    }
}

#[tokio::test]
async fn pkce_and_refresh_are_resource_bound_and_serialized() {
    let (fixture, client, worker) = fixture(false, false, false).await;
    let grant = complete(&fixture, &client).await;
    assert_eq!(
        grant
            .authorization(&client.resource.join("other").unwrap())
            .await
            .unwrap_err(),
        OAuthError::UnexpectedEndpoint
    );
    let (first, second) = tokio::join!(
        grant.authorization(&client.resource),
        grant.authorization(&client.resource)
    );
    let first = first.unwrap();
    assert_eq!(first, "Bearer refreshed-access");
    assert!(first.is_sensitive());
    assert_eq!(second.unwrap(), first);
    assert_eq!(fixture.registrations.load(Ordering::SeqCst), 1);
    assert_eq!(fixture.exchanges.load(Ordering::SeqCst), 1);
    assert_eq!(fixture.refreshes.load(Ordering::SeqCst), 1);
    worker.abort();
}

#[tokio::test]
async fn failed_refresh_is_redacted_and_never_replayed() {
    let (fixture, client, worker) = fixture(false, false, true).await;
    let grant = complete(&fixture, &client).await;
    let error = grant.authorization(&client.resource).await.unwrap_err();
    assert_eq!(error, OAuthError::TokenRejected);
    assert!(!format!("{error:?} {error}").contains("must-not-leak-token-value"));
    assert_eq!(
        grant.authorization(&client.resource).await.unwrap_err(),
        OAuthError::ReauthorizationRequired
    );
    assert_eq!(fixture.refreshes.load(Ordering::SeqCst), 1);
    worker.abort();
}

struct TestVault {
    path: std::path::PathBuf,
    key: [u8; 32],
}

impl TestVault {
    fn new() -> Self {
        Self {
            path: std::env::temp_dir().join(format!("fliuno-oauth-{}.sqlite", Uuid::new_v4())),
            key: [37; 32],
        }
    }

    fn client(&self, client: &OAuthClient, slot: &str) -> OAuthClient {
        client
            .clone()
            .with_vault(&CredentialVault::open(&self.path, &self.key).unwrap(), slot)
    }
}

impl Drop for TestVault {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[tokio::test]
async fn changing_egress_policy_prevents_attachment_and_vault_restore_of_an_old_grant() {
    let (peer, unpinned, worker) = fixture(false, false, false).await;
    let vault = TestVault::new();
    let original = complete(&peer, &vault.client(&unpinned, "remote")).await;
    let egress = EgressPolicy {
        routes: vec![crate::EgressRoute {
            host: "127.0.0.1".into(),
            addresses: vec![std::net::Ipv4Addr::LOCALHOST.into()],
            ports: vec![unpinned.resource.port().unwrap()],
        }],
    };
    let config = |egress: &EgressPolicy| -> crate::HttpServerConfig {
        serde_json::from_value(json!({"id":"remote","transport":"streamable_http","url":unpinned.resource.as_str(),"egress":egress})).unwrap()
    };
    let error = crate::connect_oauth(config(&egress), Arc::new(original))
        .err()
        .unwrap();
    assert!(error.contains("outside the configured policy"));
    assert_eq!(peer.refreshes.load(Ordering::SeqCst), 0);
    let pinned = OAuthClient::build(
        unpinned.resource.as_str(),
        &Fixture::config_at(&peer.url),
        Duration::from_secs(3),
        Some(&egress),
        true,
    )
    .unwrap();
    let pinned = vault.client(&pinned, "remote");
    assert!(pinned.restore().await.unwrap().is_none());
    let grant = complete(&peer, &pinned).await;
    assert_eq!(
        grant
            .binding_for_server(&pinned.resource, Some(&egress))
            .unwrap(),
        grant.id
    );
    assert_eq!(
        grant.authorization(&pinned.resource).await.unwrap(),
        "Bearer refreshed-access"
    );
    assert_eq!(peer.exchanges.load(Ordering::SeqCst), 2);
    assert_eq!(peer.refreshes.load(Ordering::SeqCst), 1);
    worker.abort();
}

#[test]
fn egress_requires_routes_for_configured_oauth_origins_before_discovery() {
    let mut config = Fixture::config_at("https://issuer.invalid");
    config.client_metadata_url = Some("https://client.invalid/client.json".into());
    config.authorization_endpoint_origins = vec!["https://token.invalid/".into()];
    let full: EgressPolicy = serde_json::from_value(json!({"routes":[
        {"host":"mcp.invalid","addresses":["192.0.2.1"],"ports":[443]},
        {"host":"issuer.invalid","addresses":["192.0.2.2"],"ports":[443]},
        {"host":"token.invalid","addresses":["192.0.2.3"],"ports":[443]},
        {"host":"client.invalid","addresses":["192.0.2.4"],"ports":[443]}
    ]}))
    .unwrap();
    let build = |policy: &EgressPolicy| {
        OAuthClient::new_with_egress(
            "https://mcp.invalid/mcp",
            &config,
            Duration::from_secs(1),
            policy,
        )
    };
    assert!(build(&full).is_ok());
    let mut wrong_port = full.clone();
    wrong_port.routes[0].ports = vec![8443];
    assert!(matches!(
        build(&wrong_port),
        Err(OAuthError::UnexpectedEndpoint)
    ));
    for index in 0..full.routes.len() {
        let mut missing = full.clone();
        missing.routes.remove(index);
        assert!(matches!(
            build(&missing),
            Err(OAuthError::UnexpectedEndpoint)
        ));
    }
}

#[tokio::test]
async fn encrypted_vault_restores_identity_and_rejects_an_old_grant_after_login() {
    let (peer, client, worker) = fixture(false, false, false).await;
    let vault = TestVault::new();
    let bound = vault.client(&client, "remote");
    let first = complete(&peer, &bound).await;
    let restored = vault
        .client(&client, "remote")
        .restore()
        .await
        .unwrap()
        .unwrap();
    assert_eq!(restored.id, first.id);
    assert_eq!(
        restored.authorization(&client.resource).await.unwrap(),
        "Bearer refreshed-access"
    );
    assert_eq!(
        first.authorization(&client.resource).await.unwrap(),
        "Bearer refreshed-access"
    );
    assert_eq!(peer.refreshes.load(Ordering::SeqCst), 1);
    assert!(
        vault
            .client(&client, "other")
            .restore()
            .await
            .unwrap()
            .is_none()
    );
    assert!(matches!(
        CredentialVault::open(&vault.path, &[38; 32]),
        Err(OAuthError::CredentialStore)
    ));
    let next = complete(&peer, &bound).await;
    assert_ne!(next.id, first.id);
    assert_eq!(
        first.authorization(&client.resource).await.unwrap_err(),
        OAuthError::ReauthorizationRequired
    );
    let bytes = std::fs::read(&vault.path).unwrap();
    for secret in [
        "initial-access",
        "single-use-refresh",
        "refreshed-access",
        "new-refresh",
    ] {
        assert!(!bytes.windows(secret.len()).any(|w| w == secret.as_bytes()));
    }
    worker.abort();
}

#[tokio::test]
async fn vault_consumes_refresh_before_io_and_never_retries_an_interrupted_exchange() {
    let (peer, client, worker) = fixture(false, false, false).await;
    let vault = TestVault::new();
    let first = complete(&peer, &vault.client(&client, "remote")).await;
    let second = vault
        .client(&client, "remote")
        .restore()
        .await
        .unwrap()
        .unwrap();
    peer.hold_refresh.store(true, Ordering::SeqCst);
    let target = client.resource.clone();
    let dispatch = tokio::spawn(async move { first.authorization(&target).await });
    tokio::time::timeout(Duration::from_secs(3), async {
        while peer.refreshes.load(Ordering::SeqCst) == 0 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        second.authorization(&client.resource).await.unwrap_err(),
        OAuthError::ReauthorizationRequired
    );
    dispatch.abort();
    assert!(dispatch.await.unwrap_err().is_cancelled());
    assert!(
        vault
            .client(&client, "remote")
            .restore()
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(peer.refreshes.load(Ordering::SeqCst), 1);
    worker.abort();
}

#[tokio::test]
async fn discovery_checks_issuer_and_never_downgrades_advertised_cimd_to_dcr() {
    let (peer, client, worker) = fixture(false, true, false).await;
    assert!(matches!(
        client.begin(None).await,
        Err(OAuthError::IssuerMismatch)
    ));
    assert_eq!(peer.registrations.load(Ordering::SeqCst), 0);
    worker.abort();
    let (peer, client, worker) = fixture(true, false, false).await;
    assert!(matches!(
        client.begin(None).await,
        Err(OAuthError::UnsupportedFlow)
    ));
    assert_eq!(peer.registrations.load(Ordering::SeqCst), 0);
    assert!(matches!(
        client
            .begin(Some(
                "Bearer resource_metadata=\"https://elsewhere.example/meta\""
            ))
            .await,
        Err(OAuthError::UnexpectedEndpoint)
    ));
    worker.abort();
}

#[tokio::test]
async fn callback_rejects_an_issuer_mismatch_before_the_code_exchange() {
    let (peer, client, worker) = fixture(false, false, false).await;
    let attempt = client.begin(None).await.unwrap();
    let mut callback = client.redirect.clone();
    callback.query_pairs_mut().extend_pairs([
        ("state", attempt.state.as_str()),
        ("code", "fixture-code"),
        ("iss", "https://another.example"),
    ]);
    assert!(matches!(
        attempt.finish(callback.as_str()).await,
        Err(OAuthError::InvalidCallback)
    ));
    assert_eq!(peer.exchanges.load(Ordering::SeqCst), 0);
    worker.abort();
}
