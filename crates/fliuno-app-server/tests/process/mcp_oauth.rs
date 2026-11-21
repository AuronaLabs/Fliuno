use super::{Peer, http_wire};
use axum::{
    Router,
    body::Bytes,
    extract::State,
    http::{HeaderMap, Method, StatusCode, Uri, header},
    response::{IntoResponse, Response},
};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs,
    io::Read,
    process::{Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

struct Issuer {
    origin: String,
    exchanged: AtomicUsize,
    refreshed: AtomicUsize,
    called: AtomicUsize,
    hold_refresh: bool,
}

async fn route(
    State(issuer): State<Arc<Issuer>>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let url = |path: &str| format!("{}{path}", issuer.origin);
    let result = match (method, uri.path()) {
        (Method::GET, "/.well-known/oauth-protected-resource/mcp") => {
            json!({"resource":url("/mcp"),"authorization_servers":[url("/issuer")]})
        }
        (Method::GET, "/.well-known/oauth-authorization-server/issuer") => json!({
            "issuer":url("/issuer"),"authorization_endpoint":url("/authorize"),
            "token_endpoint":url("/token"),"response_types_supported":["code"],
            "code_challenge_methods_supported":["S256"],"token_endpoint_auth_methods_supported":["none"],
            "authorization_response_iss_parameter_supported":true
        }),
        (Method::POST, "/token") => {
            let query = reqwest::Url::parse(&format!(
                "https://fixture.invalid/?{}",
                std::str::from_utf8(&body).unwrap()
            ))
            .unwrap();
            let fields: BTreeMap<_, _> = query.query_pairs().collect();
            assert_eq!(fields["client_id"], "fixture-public-client");
            assert_eq!(fields["resource"], url("/mcp"));
            if fields["grant_type"] == "authorization_code" {
                issuer.exchanged.fetch_add(1, Ordering::SeqCst);
                assert_eq!(fields["code"], "fixture-private-code");
                assert_eq!(fields["redirect_uri"], url("/callback"));
                assert!(fields["code_verifier"].len() >= 43);
                json!({"token_type":"Bearer","access_token":"fixture-initial-access","refresh_token":"fixture-private-refresh","expires_in":0})
            } else {
                issuer.refreshed.fetch_add(1, Ordering::SeqCst);
                assert_eq!(fields["grant_type"], "refresh_token");
                assert_eq!(fields["refresh_token"], "fixture-private-refresh");
                if issuer.hold_refresh {
                    std::future::pending::<()>().await;
                }
                json!({"token_type":"Bearer","access_token":"fixture-active-access","expires_in":3600})
            }
        }
        (Method::POST, "/mcp") => {
            assert_eq!(
                headers[header::AUTHORIZATION],
                "Bearer fixture-active-access"
            );
            let request: Value = serde_json::from_slice(&body).unwrap();
            let response = match request["method"].as_str().unwrap() {
                "server/discover" => {
                    json!({"resultType":"complete","supportedVersions":["2026-07-28"],
                    "capabilities":{"tools":{}},"ttlMs":0,"cacheScope":"private"})
                }
                "tools/list" => json!({"tools":[{"name":"echo","inputSchema":{"type":"object"}}]}),
                "tools/call" => {
                    issuer.called.fetch_add(1, Ordering::SeqCst);
                    json!({"content":[{"type":"text","text":"fixture-ok"}]})
                }
                _ => return StatusCode::BAD_REQUEST.into_response(),
            };
            json!({"jsonrpc":"2.0","id":request["id"],"result":response})
        }
        _ => return StatusCode::NOT_FOUND.into_response(),
    };
    (
        [(header::CONTENT_TYPE, "application/json")],
        result.to_string(),
    )
        .into_response()
}

#[tokio::test]
async fn private_startup_oauth_registers_the_tool_without_persisting_credentials() {
    startup_oauth(Startup::Memory).await;
}

#[tokio::test]
async fn encrypted_oauth_vault_restores_the_grant_and_pending_approval_after_restart() {
    startup_oauth(Startup::Restore).await;
}

#[tokio::test]
async fn killed_oauth_refresh_requires_a_new_login_without_replaying() {
    startup_oauth(Startup::InterruptedRefresh).await;
}

#[tokio::test]
async fn explicit_reauthorization_invalidates_an_approval_bound_to_the_previous_grant() {
    startup_oauth(Startup::Reauthorize).await;
}

#[derive(Clone, Copy)]
enum Startup {
    Memory,
    Restore,
    InterruptedRefresh,
    Reauthorize,
}

async fn startup_oauth(startup: Startup) {
    let persist = !matches!(startup, Startup::Memory);
    let hold_refresh = matches!(startup, Startup::InterruptedRefresh);
    let reauthorize = matches!(startup, Startup::Reauthorize);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let issuer = Arc::new(Issuer {
        origin: format!("http://{address}"),
        exchanged: AtomicUsize::new(0),
        refreshed: AtomicUsize::new(0),
        called: AtomicUsize::new(0),
        hold_refresh,
    });
    let router = Router::new().fallback(route).with_state(issuer.clone());
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    tokio::task::spawn_blocking(move || {
        let root = std::env::temp_dir().join(format!("fliuno-oauth-process-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&root).unwrap();
        let db = root.join("kernel.db");
        let vault = root.join("oauth-vault.sqlite");
        let mcp = root.join("mcp.json");
        let oauth = root.join("oauth.json");
        let url = |path: &str| format!("{}{path}", issuer.origin);
        fs::write(&mcp, json!({"servers":[{"id":"remote","transport":"streamable_http","url":url("/mcp"),"timeoutMs":2000,
            "egress":{"routes":[{"host":"127.0.0.1","addresses":["127.0.0.1"],"ports":[address.port()]}]}}]}).to_string()).unwrap();
        fs::write(&oauth, json!({"servers":{"remote":{"issuer":url("/issuer"),"redirectUri":url("/callback"),"clientId":"fixture-public-client"}}}).to_string()).unwrap();
        let model = http_wire::Fixture::start(|request| {
            let input = request.body["input"].as_array().unwrap();
            let output = if input.iter().any(|v| v["type"] == "function_call_output") {
                json!([{"type":"message","content":[{"type":"output_text","text":"completed"}]}])
            } else {
                let name = request.body["tools"].as_array().unwrap().iter().find_map(|t| {
                    t["name"].as_str().filter(|name| name.starts_with("mcp_remote_"))
                }).unwrap();
                json!([{"type":"function_call","call_id":"test-call","name":name,"arguments":"{}"}])
            };
            http_wire::Response::json(200, json!({"status":"completed","output":output}))
        });
        let mut command = Command::new(env!("CARGO_BIN_EXE_fliuno-app-server"));
        command.arg("--db").arg(&db).arg("--mcp-config").arg(&mcp)
            .arg("--mcp-oauth-config").arg(&oauth).stdin(Stdio::piped()).stdout(Stdio::piped())
            .env("FLIUNO_PROVIDER", "openai").env("FLIUNO_MODEL", "fixture-model")
            .env("OPENAI_API_KEY", "fixture-key").env("OPENAI_BASE_URL", model.url("/v1/"));
        let rejected = command.output().unwrap();
        assert!(!rejected.status.success());
        assert!(String::from_utf8_lossy(&rejected.stderr).contains("outside the configured policy"));
        assert!(!db.exists());
        if persist {
            command.arg("--mcp-oauth-vault").arg(&vault);
        }
        let mut child = command.arg("--mcp-oauth-loopback-fixture").stderr(Stdio::piped()).spawn().unwrap();
        let mut stderr = child.stderr.take().unwrap();
        let mut peer = Peer::from_child(child);
        let pending = peer.call("mcp/oauth/pending", json!({}));
        assert_eq!(pending["serverIds"], json!(["remote"]));
        assert_eq!(pending["vaultLocked"], persist);
        let key = "JSUlJSUlJSUlJSUlJSUlJSUlJSUlJSUlJSUlJSUlJSU";
        let unlock = |peer: &mut Peer| peer.call("mcp/oauth/unlock", json!({"key":key}));
        if persist {
            assert_eq!(peer.request("mcp/oauth/begin", json!({"serverId":"remote"}))["error"]["code"], -32063);
            assert_eq!(unlock(&mut peer)["remaining"], 1);
        }
        let init = json!({"protocolVersion":"0.1","clientName":"oauth-process-test"});
        assert_eq!(peer.request("initialize", init.clone())["error"]["code"], -32060);
        assert!(!db.exists());
        let begin = |peer: &mut Peer| peer.call("mcp/oauth/begin", json!({"serverId":"remote"}));
        let first = begin(&mut peer);
        assert_eq!(first, begin(&mut peer));
        let callback = |attempt: &Value, state: Option<&str>| {
            let authorize = reqwest::Url::parse(attempt["authorizationUrl"].as_str().unwrap()).unwrap();
            assert_eq!(authorize.path(), "/authorize");
            let params: BTreeMap<_, _> = authorize.query_pairs().collect();
            assert_eq!(params["code_challenge_method"], "S256");
            let mut callback = reqwest::Url::parse(&url("/callback")).unwrap();
            callback.query_pairs_mut().extend_pairs([
                ("state", state.unwrap_or(params["state"].as_ref())),
                ("iss", url("/issuer").as_str()),
                ("code", "fixture-private-code"),
            ]);
            json!({"serverId":"remote","attemptId":attempt["attemptId"],"callbackUrl":callback.as_str()})
        };
        let bad = callback(&first, Some("wrong-state"));
        let response = peer.request("mcp/oauth/finish", bad);
        assert_eq!(response["error"]["code"], -32062);
        assert!(!response.to_string().contains("fixture-private-code"));
        assert_eq!(peer.request("mcp/oauth/finish", callback(&first, None))["error"]["code"], -32602);
        assert_eq!(issuer.exchanged.load(Ordering::SeqCst), 0);
        let next = begin(&mut peer);
        assert_ne!(first["attemptId"], next["attemptId"]);
        let accepted = callback(&next, None);
        assert_eq!(peer.call("mcp/oauth/finish", accepted.clone())["remaining"], 0);
        if hold_refresh {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            while issuer.refreshed.load(Ordering::SeqCst) == 0 {
                assert!(std::time::Instant::now() < deadline, "refresh did not reach issuer");
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            drop(peer);
            let mut restarted = Peer::from_child(command.spawn().unwrap());
            assert_eq!(unlock(&mut restarted)["remaining"], 1);
            assert_eq!(restarted.call("mcp/oauth/pending", json!({}))["serverIds"], json!(["remote"]));
            assert_eq!(restarted.request("mcp/oauth/finish", accepted)["error"]["code"], -32602);
            assert_eq!(issuer.refreshed.load(Ordering::SeqCst), 1);
            assert_eq!(issuer.called.load(Ordering::SeqCst), 0);
            drop(restarted);
            fs::remove_dir_all(root).unwrap();
            return;
        }
        peer.call("initialize", init.clone());
        let workspace = peer.call("workspace/open", json!({"root":root}));
        let session = peer.call("session/create", json!({"workspaceId":workspace["id"]}));
        let task = peer.call("task/start", json!({"sessionId":session["id"],"input":"Run fixture tool"}));
        let mut task_id = task["id"].as_str().unwrap().to_owned();
        let mut waiting = peer.wait_for_status(&task_id, "waiting_approval");
        assert_eq!(issuer.called.load(Ordering::SeqCst), 0);
        let mut diagnostics = String::new();
        if persist {
            drop(peer);
            stderr.read_to_string(&mut diagnostics).unwrap();
            peer = Peer::from_child(command.spawn().unwrap());
            let wrong = "JiYmJiYmJiYmJiYmJiYmJiYmJiYmJiYmJiYmJiYmJiY";
            let error = peer.request("mcp/oauth/unlock", json!({"key":wrong}));
            assert_eq!(error["error"]["code"], -32064);
            assert!(!error.to_string().contains(wrong));
            if reauthorize {
                let invalid = peer.request("mcp/oauth/unlock", json!({"key":key,"reauthorizeServerIds":["missing"]}));
                assert_eq!(invalid["error"]["code"], -32602);
                assert_eq!(peer.call("mcp/oauth/pending", json!({}))["vaultLocked"], true);
                assert_eq!(peer.call("mcp/oauth/unlock", json!({"key":key,"reauthorizeServerIds":["remote"]}))["remaining"], 1);
                assert_eq!(peer.request("initialize", init.clone())["error"]["code"], -32060);
                assert_eq!(peer.request("mcp/oauth/finish", accepted.clone())["error"]["code"], -32602);
                let fresh = begin(&mut peer);
                assert_ne!(next["attemptId"], fresh["attemptId"]);
                assert_eq!(peer.call("mcp/oauth/finish", callback(&fresh, None))["remaining"], 0);
            } else {
                assert_eq!(unlock(&mut peer)["remaining"], 0);
            }
            peer.call("initialize", init);
            assert_eq!(peer.call("task/get", json!({"taskId":task_id}))["pendingApproval"]["id"], waiting["pendingApproval"]["id"]);
            if reauthorize {
                peer.call("approval/respond", json!({"taskId":task_id,"approvalId":waiting["pendingApproval"]["id"],"decision":"allow_once"}));
                let rejected = peer.wait_for_status(&task_id, "failed");
                assert!(rejected["output"].as_str().unwrap().contains("registration changed"));
                assert_eq!(issuer.called.load(Ordering::SeqCst), 0);
                let fresh = peer.call("task/start", json!({"sessionId":session["id"],"input":"Run fixture tool under the new grant"}));
                task_id = fresh["id"].as_str().unwrap().to_owned();
                waiting = peer.wait_for_status(&task_id, "waiting_approval");
            }
        }
        peer.call("approval/respond", json!({"taskId":task_id,"approvalId":waiting["pendingApproval"]["id"],"decision":"allow_once"}));
        let completed = peer.wait_for_status(&task_id, "completed");
        assert_eq!(completed["output"], "completed");
        let events = peer.call("events/list", json!({"taskId":task_id}));
        assert_eq!(issuer.called.load(Ordering::SeqCst), 1);
        let logins = if reauthorize { 2 } else { 1 };
        assert_eq!(issuer.exchanged.load(Ordering::SeqCst), logins);
        assert_eq!(issuer.refreshed.load(Ordering::SeqCst), logins);
        drop(peer);
        if !persist {
            stderr.read_to_string(&mut diagnostics).unwrap();
        }
        let mut restarted = Peer::from_child(command.spawn().unwrap());
        assert_eq!(restarted.call("mcp/oauth/pending", json!({}))["serverIds"], json!(["remote"]));
        assert_eq!(restarted.request("mcp/oauth/finish", accepted)["error"]["code"], if persist { -32063 } else { -32602 });
        assert_eq!(issuer.exchanged.load(Ordering::SeqCst), logins);
        drop(restarted);
        for entry in fs::read_dir(&root).unwrap().flatten().filter(|entry| {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            name.starts_with("kernel.db") || name.starts_with("oauth-vault.sqlite")
        }) {
            let data = fs::read(entry.path()).unwrap();
            for private in ["fixture-private-code","fixture-initial-access","fixture-private-refresh","fixture-active-access",key] {
                assert!(!data.windows(private.len()).any(|bytes| bytes == private.as_bytes()));
                assert!(!events.to_string().contains(private));
                assert!(!diagnostics.contains(private));
            }
        }
        fs::remove_dir_all(root).unwrap();
    }).await.unwrap();
    server.abort();
}
