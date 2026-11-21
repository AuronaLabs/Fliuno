//! Private stdio bootstrap, before SQLite, tool registration and subscriptions.
use crate::{InputFrame, write_frame};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use crossbeam_channel::{Receiver, RecvTimeoutError};
use fliuno_mcp::oauth::{AuthorizationAttempt, CredentialVault, OAuthClient, OAuthGrant};
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::Write,
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};
use uuid::Uuid;
use zeroize::Zeroizing;

type Clients = BTreeMap<String, OAuthClient>;
type Grants = BTreeMap<String, Arc<OAuthGrant>>;
const STARTUP_DEADLINE: Duration = Duration::from_secs(600);

struct Pending {
    id: String,
    attempt: AuthorizationAttempt,
}

struct Bootstrap {
    clients: Clients,
    pending: BTreeMap<String, Pending>,
    grants: Grants,
    locked_vault: Option<PathBuf>,
}

pub(super) fn authorize(
    clients: Clients,
    locked_vault: Option<PathBuf>,
    input: &Receiver<InputFrame>,
    output: &mut impl Write,
) -> Result<Grants, Box<dyn std::error::Error>> {
    let mut bootstrap = Bootstrap {
        clients,
        pending: BTreeMap::new(),
        grants: BTreeMap::new(),
        locked_vault,
    };
    if bootstrap.clients.is_empty() {
        return Ok(bootstrap.grants);
    }
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let deadline = Instant::now() + STARTUP_DEADLINE;
    while !bootstrap.clients.is_empty() {
        let response = match input.recv_deadline(deadline) {
            Ok(InputFrame::Line(line)) => runtime
                .block_on(async {
                    tokio::time::timeout_at(deadline.into(), bootstrap.handle(&line)).await
                })
                .map_err(|_| "MCP OAuth bootstrap timed out")?,
            Ok(InputFrame::InvalidUtf8) => failure(Value::Null, -32700, "parse error"),
            Ok(InputFrame::TooLarge) => return Err("RPC frame exceeds 1 MiB".into()),
            Err(RecvTimeoutError::Timeout) => return Err("MCP OAuth bootstrap timed out".into()),
            Err(RecvTimeoutError::Disconnected) => {
                return Err("MCP OAuth bootstrap was closed".into());
            }
        };
        write_frame(output, &response)?;
    }
    Ok(bootstrap.grants)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Begin {
    server_id: String,
    www_authenticate: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Finish {
    server_id: String,
    attempt_id: String,
    callback_url: String,
}

type RpcError = (i32, &'static str);

impl Bootstrap {
    async fn handle(&mut self, line: &str) -> Value {
        let Ok(Value::Object(mut request)) = serde_json::from_str::<Value>(line) else {
            return failure(
                Value::Null,
                -32600,
                "MCP OAuth requires a single request object",
            );
        };
        let Some(id) = request.remove("id").filter(|id| {
            id.is_i64() || id.is_u64() || id.as_str().is_some_and(|id| id.len() <= 128)
        }) else {
            return failure(Value::Null, -32600, "MCP OAuth requires a request ID");
        };
        if request.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
            return failure(Value::Null, -32600, "invalid JSON-RPC version");
        }
        let method = request.remove("method");
        let params = request.remove("params").unwrap_or_else(|| json!({}));
        let result = match method.as_ref().and_then(Value::as_str) {
            Some("mcp/oauth/pending") if params.as_object().is_some_and(|p| p.is_empty()) => {
                Ok(json!({"serverIds":self.clients.keys().collect::<Vec<_>>(),
                    "vaultLocked":self.locked_vault.is_some() }))
            }
            Some("mcp/oauth/pending") => Err((-32602, "invalid MCP OAuth params")),
            Some("mcp/oauth/unlock") => self.unlock(params).await,
            Some(_) if self.locked_vault.is_some() => Err((-32063, "MCP OAuth vault is locked")),
            Some("mcp/oauth/begin") => self.begin(params).await,
            Some("mcp/oauth/finish") => self.finish(params).await,
            Some(_) => Err((-32060, "MCP OAuth bootstrap is pending")),
            None => Err((-32600, "invalid JSON-RPC method")),
        };
        match result {
            Ok(result) => json!({"jsonrpc":"2.0","id":id,"result":result}),
            Err((code, message)) => failure(id, code, message),
        }
    }

    async fn unlock(&mut self, params: Value) -> Result<Value, RpcError> {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase", deny_unknown_fields)]
        struct Unlock {
            key: String,
            #[serde(default)]
            reauthorize_server_ids: Vec<String>,
        }

        let path = self
            .locked_vault
            .clone()
            .ok_or((-32602, "no locked MCP OAuth vault"))?;
        let params: Unlock = decode(params)?;
        let encoded = Zeroizing::new(params.key);
        let reauthorize: BTreeSet<_> = params.reauthorize_server_ids.iter().collect();
        if reauthorize.len() != params.reauthorize_server_ids.len()
            || reauthorize.iter().any(|id| !self.clients.contains_key(*id))
        {
            return Err((-32602, "invalid MCP OAuth reauthorization selection"));
        }
        let mut key = Zeroizing::new([0_u8; 32]);
        if encoded.len() != 43
            || URL_SAFE_NO_PAD
                .decode_slice(encoded.as_bytes(), &mut *key)
                .ok()
                != Some(32)
        {
            return Err((-32602, "invalid MCP OAuth vault key encoding"));
        }
        let error = (-32064, "MCP OAuth vault could not be opened or restored");
        let vault = tokio::task::spawn_blocking(move || CredentialVault::open(&path, &key))
            .await
            .map_err(|_| error)?
            .map_err(|_| error)?;
        let mut pending = BTreeMap::new();
        let mut grants = BTreeMap::new();
        for (id, client) in &self.clients {
            let client = client.clone().with_vault(&vault, id);
            if !reauthorize.contains(id)
                && let Some(grant) = client.restore().await.map_err(|_| error)?
            {
                grants.insert(id.clone(), Arc::new(grant));
            } else {
                pending.insert(id.clone(), client);
            }
        }
        self.clients = pending;
        self.grants = grants;
        self.locked_vault = None;
        Ok(json!({"remaining":self.clients.len()}))
    }

    async fn begin(&mut self, params: Value) -> Result<Value, RpcError> {
        let params: Begin = decode(params)?;
        let client = self
            .clients
            .get(&params.server_id)
            .ok_or((-32602, "no pending MCP OAuth server"))?;
        if let Some(pending) = self.pending.get(&params.server_id) {
            return Ok(authorization_url(pending));
        }
        let attempt = client
            .begin(params.www_authenticate.as_deref())
            .await
            .map_err(|_| (-32061, "MCP OAuth discovery or registration failed"))?;
        let pending = Pending {
            id: Uuid::new_v4().to_string(),
            attempt,
        };
        let reply = authorization_url(&pending);
        self.pending.insert(params.server_id, pending);
        Ok(reply)
    }

    async fn finish(&mut self, params: Value) -> Result<Value, RpcError> {
        let params: Finish = decode(params)?;
        if !self
            .pending
            .get(&params.server_id)
            .is_some_and(|p| p.id == params.attempt_id)
        {
            return Err((-32602, "no matching MCP OAuth attempt"));
        }
        let attempt = self
            .pending
            .remove(&params.server_id)
            .expect("matched attempt")
            .attempt;
        let grant = attempt
            .finish(&params.callback_url)
            .await
            .map_err(|_| (-32062, "MCP OAuth callback or token exchange failed"))?;
        self.clients.remove(&params.server_id);
        self.grants
            .insert(params.server_id.clone(), Arc::new(grant));
        Ok(json!({"serverId":params.server_id,"remaining":self.clients.len()}))
    }
}

fn authorization_url(pending: &Pending) -> Value {
    json!({"attemptId":pending.id,"authorizationUrl":pending.attempt.authorization_url().as_str()})
}

fn decode<T: DeserializeOwned>(params: Value) -> Result<T, RpcError> {
    serde_json::from_value(params).map_err(|_| (-32602, "invalid MCP OAuth params"))
}

fn failure(id: Value, code: i32, message: &str) -> Value {
    json!({"jsonrpc":"2.0","id":id,"error":{"code":code,"message":message}})
}
