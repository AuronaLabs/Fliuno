//! Opt-in host transport for workspace embeddings. No DB lock spans HTTP.
mod centering;

use centering::Centers;
use crossbeam_channel::{Receiver, Sender, bounded, select_biased, tick};
use fliuno_core::{Kernel, TaskMemoryQuery};
use fliuno_protocol::{
    MAX_MEMORY_EMBEDDING_DIMENSIONS, MAX_MEMORY_SEMANTIC_RESULTS, TaskInput, UserContentPart,
    WorkspaceMemoryIndexParams, WorkspaceMemoryQueryEmbedding, WorkspaceMemoryVector,
};
use reqwest::{Url, header::HeaderValue};
use serde::Deserialize;
use std::{
    collections::BTreeSet,
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

const QUERY_BYTES: usize = 4096;
const RESPONSE_BYTES: usize = 1024 * 1024;
const INDEX_INTERVAL: Duration = Duration::from_secs(1);
const QUERY_WAIT: Duration = Duration::from_millis(750);
const E5_INDEX_PREFIX: &str = "e5-en-zh-v1:";

#[derive(Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum EmbeddingProfile {
    #[default]
    Plain,
    MultilingualE5EnZhV1,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MemoryEmbeddingConfig {
    endpoint: String,
    model: String,
    model_identity: String,
    dimensions: usize,
    api_key_env: Option<String>,
    workspaces: Vec<String>,
    #[serde(default)]
    profile: EmbeddingProfile,
    minimum_similarity: Option<f64>,
}

pub struct MemoryEmbeddingProvider {
    config: MemoryEmbeddingConfig,
    endpoint: Url,
    client: reqwest::Client,
    minimum_similarity: f64,
    result_limit: u32,
}

impl MemoryEmbeddingProvider {
    pub fn new(mut config: MemoryEmbeddingConfig) -> Result<Self, &'static str> {
        const INVALID: &str = "invalid workspace memory embedding configuration";
        let centered = config.profile == EmbeddingProfile::MultilingualE5EnZhV1;
        let minimum_similarity =
            config
                .minimum_similarity
                .unwrap_or(if centered { 0.24 } else { 0.65 });
        let endpoint = Url::parse(&config.endpoint).map_err(|_| INVALID)?;
        let local = endpoint.host_str().is_some_and(|host| {
            host.trim_matches(['[', ']'])
                .parse::<std::net::IpAddr>()
                .is_ok_and(|ip| ip.is_loopback())
        });
        let valid = |s: &str| {
            !s.is_empty() && s.len() <= 128 && s.trim() == s && !s.chars().any(char::is_control)
        };
        if endpoint.host().is_none()
            || !(endpoint.scheme() == "https" || local && endpoint.scheme() == "http")
            || !endpoint.username().is_empty()
            || endpoint.password().is_some()
            || endpoint.query().is_some()
            || endpoint.fragment().is_some()
            || config.endpoint.len() > 4096
            || config.endpoint.chars().any(char::is_control)
            || !valid(&config.model)
            || !valid(&config.model_identity)
            || !(1..=MAX_MEMORY_EMBEDDING_DIMENSIONS).contains(&config.dimensions)
            || config.model_identity.starts_with(E5_INDEX_PREFIX)
            || (centered && config.model_identity.len() + E5_INDEX_PREFIX.len() > 128)
            || !(0.0 < minimum_similarity && minimum_similarity <= 1.0)
            || !(1..=32).contains(&config.workspaces.len())
            || !config
                .workspaces
                .iter()
                .all(|s| valid(s) && s.starts_with("workspace_"))
            || config.workspaces.iter().collect::<BTreeSet<_>>().len() != config.workspaces.len()
        {
            return Err(INVALID);
        }
        let mut headers = reqwest::header::HeaderMap::new();
        if let Some(name) = &config.api_key_env {
            if !valid(name) || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
                return Err(INVALID);
            }
            let key = std::env::var(name).map_err(|_| "memory embedding API key is unavailable")?;
            if key.is_empty() || key.len() > 8192 || key.chars().any(char::is_whitespace) {
                return Err(INVALID);
            }
            let mut header =
                HeaderValue::from_str(&format!("Bearer {key}")).map_err(|_| INVALID)?;
            header.set_sensitive(true);
            headers.insert(reqwest::header::AUTHORIZATION, header);
        }
        let client = reqwest::Client::builder()
            .default_headers(headers)
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .no_proxy()
            .timeout(Duration::from_secs(2))
            .build()
            .map_err(|_| INVALID)?;
        if centered {
            config.model_identity.insert_str(0, E5_INDEX_PREFIX);
        }
        Ok(Self {
            config,
            endpoint,
            client,
            minimum_similarity,
            result_limit: if centered {
                1
            } else {
                MAX_MEMORY_SEMANTIC_RESULTS
            },
        })
    }

    async fn embed(&self, inputs: &[String]) -> Option<Vec<Vec<f32>>> {
        let mut response = self
            .client
            .post(self.endpoint.clone())
            .json(&serde_json::json!({
                "model": self.config.model, "input":inputs, "dimensions": self.config.dimensions,
                "encoding_format":"float",
            }))
            .send()
            .await
            .ok()?;
        if !response.status().is_success()
            || response
                .content_length()
                .is_some_and(|n| n > RESPONSE_BYTES as u64)
        {
            return None;
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.ok()? {
            if bytes.len() + chunk.len() > RESPONSE_BYTES {
                return None;
            }
            bytes.extend_from_slice(&chunk);
        }
        #[derive(Deserialize)]
        struct Batch {
            data: Vec<Row>,
        }
        #[derive(Deserialize)]
        struct Row {
            index: usize,
            embedding: Vec<f32>,
        }
        let mut rows = serde_json::from_slice::<Batch>(&bytes).ok()?.data;
        rows.sort_unstable_by_key(|row| row.index);
        if rows.len() != inputs.len()
            || rows.iter().enumerate().any(|(i, row)| {
                i != row.index
                    || row.embedding.len() != self.config.dimensions
                    || !row.embedding.iter().all(|v| v.is_finite())
                    || !row.embedding.iter().any(|v| *v != 0.0)
            })
        {
            return None;
        }
        Some(rows.into_iter().map(|row| row.embedding).collect())
    }

    async fn index_batch(
        &self,
        kernel: &Mutex<Kernel>,
        workspace_id: &str,
        centers: Option<&Centers>,
    ) {
        let Ok(rows) = kernel.lock().map_err(|_| ()).and_then(|kernel| {
            kernel
                .pending_workspace_memory_vectors(
                    workspace_id,
                    &self.config.model_identity,
                    self.config.dimensions,
                )
                .map_err(|_| ())
        }) else {
            return;
        };
        if rows.is_empty() {
            return;
        }
        let texts: Vec<_> = rows
            .iter()
            .map(|row| {
                let prefix = if centers.is_some() { "passage: " } else { "" };
                format!("{prefix}{}\n{}", row.key, row.content)
            })
            .collect();
        let Some(mut vectors) = self.embed(&texts).await else {
            return;
        };
        if let Some(centers) = centers {
            for (vector, row) in vectors.iter_mut().zip(&rows) {
                if !centers.transform(vector, &row.content, centering::Role::Passage) {
                    return;
                }
            }
        }
        if let Ok(mut kernel) = kernel.lock() {
            let _ = kernel.index_workspace_memory_vectors(&WorkspaceMemoryIndexParams {
                workspace_id: workspace_id.into(),
                model_identity: self.config.model_identity.clone(),
                vectors: rows
                    .into_iter()
                    .zip(vectors)
                    .map(|(row, values)| WorkspaceMemoryVector {
                        memory_id: row.id,
                        revision: row.revision,
                        values,
                    })
                    .collect(),
            });
        }
    }
}

struct Query {
    text: String,
    expires: Instant,
    reply: Sender<Option<Vec<f32>>>,
}

pub(crate) struct Service {
    workspaces: BTreeSet<String>,
    model_identity: String,
    minimum_similarity: f64,
    result_limit: u32,
    requests: Option<Sender<Query>>,
    worker: Option<thread::JoinHandle<()>>,
}

impl Service {
    pub(crate) fn start(
        provider: MemoryEmbeddingProvider,
        kernel: Arc<Mutex<Kernel>>,
    ) -> std::io::Result<Self> {
        let (submit, requests) = bounded(1);
        let mut service = Self {
            workspaces: provider.config.workspaces.iter().cloned().collect(),
            model_identity: provider.config.model_identity.clone(),
            minimum_similarity: provider.minimum_similarity,
            result_limit: provider.result_limit,
            requests: Some(submit),
            worker: None,
        };
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        service.worker = Some(
            thread::Builder::new()
                .name("fliuno-memory-embeddings".into())
                .spawn(move || serve(provider, &kernel, &runtime, &requests))?,
        );
        Ok(service)
    }

    pub(crate) fn query(&self, workspace_id: String, input: &TaskInput) -> Retrieved {
        if !self.workspaces.contains(&workspace_id) {
            return Retrieved::Keyword;
        }
        let text = query_text(input);
        if text.trim().is_empty() {
            return Retrieved::Keyword;
        }
        let (reply, result) = bounded(1);
        let expires = Instant::now() + QUERY_WAIT;
        if self.requests.as_ref().is_none_or(|requests| {
            requests
                .try_send(Query {
                    text,
                    expires,
                    reply,
                })
                .is_err()
        }) {
            return Retrieved::Unavailable;
        }
        let Some(values) = result.recv_deadline(expires).ok().flatten() else {
            return Retrieved::Unavailable;
        };
        Retrieved::Semantic {
            workspace_id,
            minimum_similarity: self.minimum_similarity,
            result_limit: self.result_limit,
            embedding: WorkspaceMemoryQueryEmbedding {
                model_identity: self.model_identity.clone(),
                values,
            },
        }
    }
}

impl Drop for Service {
    fn drop(&mut self) {
        drop(self.requests.take());
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn serve(
    provider: MemoryEmbeddingProvider,
    kernel: &Mutex<Kernel>,
    runtime: &tokio::runtime::Runtime,
    requests: &Receiver<Query>,
) {
    let ticker = tick(INDEX_INTERVAL);
    let mut workspaces = provider.config.workspaces.iter().cycle();
    let mut centers: Option<Centers> = None;
    let centered = provider.config.profile == EmbeddingProfile::MultilingualE5EnZhV1;
    loop {
        select_biased! {
            recv(requests) -> request => {
                let Ok(Query { text, expires, reply }) = request else { break; };
                if centered && centers.is_none() {
                    let _ = reply.try_send(None);
                    continue;
                }
                if let Some(remaining) = expires.checked_duration_since(Instant::now()) {
                    let result = runtime.block_on(async {
                        let sent = if centered { format!("query: {}", text.trim_end_matches('\n')) } else { text.clone() };
                        let mut vector = tokio::time::timeout(remaining, provider.embed(&[sent])).await.ok().flatten()?.pop()?;
                        if let Some(centers) = &centers
                            && !centers.transform(&mut vector, &text, centering::Role::Query) {
                            return None;
                        }
                        Some(vector)
                    });
                    let _ = reply.try_send(result);
                }
            }
            recv(ticker) -> _ => {
                if centered && centers.is_none() {
                    centers = runtime.block_on(Centers::load(&provider));
                }
                if !centered || centers.is_some() {
                    runtime.block_on(provider.index_batch(kernel, workspaces.next().unwrap(), centers.as_ref()));
                }
            }
        }
    }
}

fn query_text(input: &TaskInput) -> String {
    let mut output = String::new();
    let mut append = |text: &str| {
        let mut take = text.len().min(QUERY_BYTES.saturating_sub(output.len()));
        while !text.is_char_boundary(take) {
            take -= 1;
        }
        output.push_str(&text[..take]);
        if output.len() < QUERY_BYTES {
            output.push('\n');
        }
    };
    match input {
        TaskInput::Text(text) => append(text),
        TaskInput::Parts(parts) => {
            for part in parts {
                if let UserContentPart::Text { text } = part {
                    append(text);
                }
            }
        }
    }
    output
}

#[derive(Default)]
pub(crate) enum Retrieved {
    #[default]
    Keyword,
    Unavailable,
    Semantic {
        workspace_id: String,
        embedding: WorkspaceMemoryQueryEmbedding,
        minimum_similarity: f64,
        result_limit: u32,
    },
}

impl Retrieved {
    pub(crate) fn query(&self) -> TaskMemoryQuery<'_> {
        match self {
            Self::Keyword => TaskMemoryQuery::Keyword,
            Self::Unavailable => TaskMemoryQuery::Unavailable,
            Self::Semantic {
                workspace_id,
                embedding,
                minimum_similarity,
                result_limit,
            } => TaskMemoryQuery::Semantic {
                workspace_id,
                embedding,
                minimum_similarity: *minimum_similarity,
                result_limit: *result_limit,
            },
        }
    }
}
