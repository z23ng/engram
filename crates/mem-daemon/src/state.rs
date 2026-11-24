//! Daemon runtime configuration and shared thread-safe state.

use std::collections::HashMap;
use std::net::{TcpStream, ToSocketAddrs};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use mem_search::{
    ConsolidateLlmConfig, EmbeddingProviderConfig, MemSearchEngine, OpenAiCompatibleEmbedder,
    ProviderBackend, SearchConfig,
};
use mem_store::SqliteStore;
use mem_vcs::{MemVcs, VersionControlEngine};
use tokio::sync::{broadcast, mpsc, RwLock};

use crate::events::MemoryEvent;

/// Configuration for launching the `mem-daemon` HTTP/SSE/MCP server.
#[derive(Debug, Clone)]
pub struct DaemonConfig {
    /// Host address to bind (default `"127.0.0.1"`).
    pub host: String,
    /// TCP port to bind (default `7700`; `0` lets OS assign an ephemeral port).
    pub port: u16,
    /// Single-file SQLite WAL database path.
    pub db_path: PathBuf,
    /// Embedding provider configuration.
    pub embedding: EmbeddingProviderConfig,
    /// Cold-path LLM consolidation configuration.
    pub consolidate_llm: ConsolidateLlmConfig,
    /// Optional Bearer authentication token (`None` disables auth for local loopback use).
    pub auth_token: Option<String>,
    /// Optional periodic background consolidation interval in seconds (`None` disables timer).
    pub consolidate_interval_secs: Option<u64>,
    /// Whether periodic background consolidation auto-merges clean results into `main`.
    pub consolidate_auto_merge: bool,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            host: "127.0.0.1".to_string(),
            port: 7700,
            db_path: PathBuf::from("memory.db"),
            embedding: EmbeddingProviderConfig::disabled(),
            consolidate_llm: ConsolidateLlmConfig::disabled(),
            auth_token: None,
            consolidate_interval_secs: None,
            consolidate_auto_merge: false,
        }
    }
}

/// Shared state passed across all Axum HTTP, SSE, and MCP handlers.
#[derive(Clone)]
pub struct DaemonState {
    pub vcs: MemVcs,
    pub search: MemSearchEngine,
    pub llm_cfg: ConsolidateLlmConfig,
    pub emb_cfg: EmbeddingProviderConfig,
    pub db_path: PathBuf,
    pub event_tx: broadcast::Sender<MemoryEvent>,
    pub auth_token: Option<String>,
    pub mcp_sessions: Arc<RwLock<HashMap<String, mpsc::Sender<String>>>>,
    pub started_at_ms: i64,
}

impl DaemonState {
    /// Opens or initializes the SQLite repository and builds shared daemon state.
    pub fn from_config(cfg: &DaemonConfig) -> mem_core::Result<Self> {
        if let Some(parent) = cfg.db_path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).map_err(|e| {
                    mem_core::MemError::Storage(format!(
                        "failed to create database directory '{}': {e}",
                        parent.display()
                    ))
                })?;
            }
        }

        let store = SqliteStore::open(&cfg.db_path)?;
        let vcs = MemVcs::new(store);
        vcs.init()?;

        let (search, _notice) = build_search_engine_for_config(vcs.clone(), &cfg.embedding);
        let (event_tx, _) = broadcast::channel(256);

        Ok(Self {
            vcs,
            search,
            llm_cfg: cfg.consolidate_llm.clone(),
            emb_cfg: cfg.embedding.clone(),
            db_path: cfg.db_path.clone(),
            event_tx,
            auth_token: cfg.auth_token.clone().filter(|s| !s.trim().is_empty()),
            mcp_sessions: Arc::new(RwLock::new(HashMap::new())),
            started_at_ms: Utc::now().timestamp_millis(),
        })
    }

    /// Creates daemon state from an already-initialized [`MemVcs`] and [`MemSearchEngine`].
    pub fn from_engines(
        vcs: MemVcs,
        search: MemSearchEngine,
        emb_cfg: EmbeddingProviderConfig,
        llm_cfg: ConsolidateLlmConfig,
        db_path: PathBuf,
        auth_token: Option<String>,
    ) -> Self {
        let (event_tx, _) = broadcast::channel(256);
        Self {
            vcs,
            search,
            llm_cfg,
            emb_cfg,
            db_path,
            event_tx,
            auth_token: auth_token.filter(|s| !s.trim().is_empty()),
            mcp_sessions: Arc::new(RwLock::new(HashMap::new())),
            started_at_ms: Utc::now().timestamp_millis(),
        }
    }

    /// Emits a structured [`MemoryEvent`] to any connected `/v1/events` SSE subscribers.
    pub fn emit(&self, event: &str, branch: impl Into<String>, data: serde_json::Value) {
        let _ = self.event_tx.send(MemoryEvent::new(event, branch, data));
    }
}

/// Builds a [`MemSearchEngine`] that degrades gracefully to Level-1 BM25 + Wiki Graph
/// if local Ollama is unreachable or OpenAI key is unset.
pub fn build_search_engine_for_config(
    vcs: MemVcs,
    emb_cfg: &EmbeddingProviderConfig,
) -> (MemSearchEngine, Option<String>) {
    match emb_cfg.backend {
        ProviderBackend::Disabled => (MemSearchEngine::new_bm25_only(vcs), None),
        ProviderBackend::Ollama => {
            if is_http_reachable(&emb_cfg.base_url, Duration::from_millis(400)) {
                let embedder = Arc::new(OpenAiCompatibleEmbedder::new(emb_cfg.clone()));
                (
                    MemSearchEngine::new(vcs, embedder, SearchConfig::default()),
                    None,
                )
            } else {
                let notice = format!(
                    "Ollama endpoint '{}' is unreachable; falling back to Level-1 FTS5 BM25 + Obsidian Wiki Graph.",
                    emb_cfg.base_url
                );
                (MemSearchEngine::new_bm25_only(vcs), Some(notice))
            }
        }
        ProviderBackend::OpenAi => {
            let has_key = emb_cfg
                .api_key
                .as_deref()
                .is_some_and(|k| !k.trim().is_empty());
            if has_key {
                let embedder = Arc::new(OpenAiCompatibleEmbedder::new(emb_cfg.clone()));
                (
                    MemSearchEngine::new(vcs, embedder, SearchConfig::default()),
                    None,
                )
            } else {
                let notice = "OPENAI_API_KEY is not set; falling back to Level-1 FTS5 BM25 + Obsidian Wiki Graph.".to_string();
                (MemSearchEngine::new_bm25_only(vcs), Some(notice))
            }
        }
    }
}

fn is_http_reachable(base_url: &str, timeout: Duration) -> bool {
    let without_scheme = base_url
        .trim()
        .strip_prefix("http://")
        .or_else(|| base_url.trim().strip_prefix("https://"))
        .unwrap_or(base_url.trim());
    let host_port = without_scheme.split('/').next().unwrap_or("");
    if host_port.is_empty() {
        return false;
    }
    let addr = if host_port.contains(':') {
        host_port.to_string()
    } else if base_url.trim().starts_with("https://") {
        format!("{host_port}:443")
    } else {
        format!("{host_port}:80")
    };
    let Ok(mut addrs) = addr.to_socket_addrs() else {
        return false;
    };
    let Some(sock) = addrs.next() else {
        return false;
    };
    TcpStream::connect_timeout(&sock, timeout).is_ok()
}
