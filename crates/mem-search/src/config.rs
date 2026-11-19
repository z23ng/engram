//! Provider and search configuration supporting local Ollama (dev) and OpenAI (production).

use serde::{Deserialize, Serialize};

/// Execution backend for embedding and consolidation model endpoints.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProviderBackend {
    /// Level-1 zero-model mode (pure FTS5 BM25 + Obsidian Wiki graph).
    Disabled,
    /// Local Ollama OpenAI-compatible server (`http://127.0.0.1:11434/v1`).
    Ollama,
    /// Production OpenAI API (`https://api.openai.com/v1`).
    OpenAi,
}

impl ProviderBackend {
    /// Parses `"disabled"` / `"none"`, `"ollama"`, or `"openai"` (case-insensitive).
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "disabled" | "none" | "off" => Some(Self::Disabled),
            "ollama" | "local" => Some(Self::Ollama),
            "openai" | "prod" | "production" => Some(Self::OpenAi),
            _ => None,
        }
    }
}

/// Configuration for the embedding endpoint (`Ollama` in local dev, `OpenAI` in production).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EmbeddingProviderConfig {
    /// Selected provider backend.
    pub backend: ProviderBackend,
    /// OpenAI-compatible base URL (e.g. `http://127.0.0.1:11434/v1` or `https://api.openai.com/v1`).
    pub base_url: String,
    /// Embedding model name (`bge-m3` for Ollama, `text-embedding-3-small` for OpenAI).
    pub model: String,
    /// Optional Bearer API key (`None` for local Ollama, `Some(...)` for OpenAI).
    pub api_key: Option<String>,
    /// Optional target embedding dimension passed in OpenAI-compatible payloads.
    pub dimensions: Option<usize>,
}

impl Default for EmbeddingProviderConfig {
    fn default() -> Self {
        Self::ollama_default()
    }
}

impl EmbeddingProviderConfig {
    /// Default local development configuration targeting Ollama `bge-m3`.
    pub fn ollama_default() -> Self {
        Self {
            backend: ProviderBackend::Ollama,
            base_url: "http://127.0.0.1:11434/v1".to_string(),
            model: "bge-m3".to_string(),
            api_key: None,
            dimensions: Some(1024),
        }
    }

    /// Production configuration targeting OpenAI `text-embedding-3-small`.
    pub fn openai_production(api_key: impl Into<String>) -> Self {
        Self {
            backend: ProviderBackend::OpenAi,
            base_url: "https://api.openai.com/v1".to_string(),
            model: "text-embedding-3-small".to_string(),
            api_key: Some(api_key.into()),
            dimensions: Some(1536),
        }
    }

    /// Level-1 zero-model fallback configuration (FTS5 BM25 + Obsidian Wiki graph only).
    pub fn disabled() -> Self {
        Self {
            backend: ProviderBackend::Disabled,
            base_url: String::new(),
            model: String::new(),
            api_key: None,
            dimensions: None,
        }
    }

    /// Loads embedding provider configuration from process environment variables.
    ///
    /// Variables:
    /// - `MEM_PROVIDER`: `ollama` (default) | `openai` | `disabled`
    /// - `MEM_EMBEDDING_BASE_URL`: overrides `base_url`
    /// - `MEM_EMBEDDING_MODEL`: overrides `model`
    /// - `OPENAI_API_KEY`: sets `api_key`
    /// - `MEM_EMBEDDING_DIMENSIONS`: overrides `dimensions`
    pub fn from_env() -> Self {
        Self::from_env_lookup(|key| std::env::var(key).ok())
    }

    /// Loads configuration using a custom environment lookup closure (useful for testing).
    pub fn from_env_lookup<F>(get: F) -> Self
    where
        F: Fn(&str) -> Option<String>,
    {
        let backend = get("MEM_PROVIDER")
            .and_then(|v| ProviderBackend::parse(&v))
            .unwrap_or(ProviderBackend::Ollama);

        let mut cfg = match backend {
            ProviderBackend::Disabled => Self::disabled(),
            ProviderBackend::Ollama => Self::ollama_default(),
            ProviderBackend::OpenAi => {
                let key = get("OPENAI_API_KEY").filter(|k| !k.trim().is_empty());
                Self {
                    api_key: key,
                    ..Self::openai_production("")
                }
            }
        };

        if let Some(url) = get("MEM_EMBEDDING_BASE_URL").filter(|s| !s.trim().is_empty()) {
            cfg.base_url = url;
        }
        if let Some(model) = get("MEM_EMBEDDING_MODEL").filter(|s| !s.trim().is_empty()) {
            cfg.model = model;
        }
        if let Some(key) = get("OPENAI_API_KEY").filter(|s| !s.trim().is_empty()) {
            if cfg.backend != ProviderBackend::Disabled {
                cfg.api_key = Some(key);
            }
        }
        if let Some(dim) = get("MEM_EMBEDDING_DIMENSIONS").and_then(|s| s.trim().parse::<usize>().ok())
        {
            cfg.dimensions = Some(dim);
        }

        cfg
    }
}

/// Configuration for the optional cold-path consolidation LLM (`Ollama` dev / `OpenAI` prod).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConsolidateLlmConfig {
    /// Selected provider backend.
    pub backend: ProviderBackend,
    /// OpenAI-compatible base URL.
    pub base_url: String,
    /// Chat completion model (`qwen2.5:7b` for Ollama, `gpt-6-luna` for OpenAI).
    pub model: String,
    /// Optional Bearer API key.
    pub api_key: Option<String>,
}

impl Default for ConsolidateLlmConfig {
    fn default() -> Self {
        Self::ollama_default()
    }
}

impl ConsolidateLlmConfig {
    /// Default local development configuration targeting Ollama `qwen2.5:7b`.
    pub fn ollama_default() -> Self {
        Self {
            backend: ProviderBackend::Ollama,
            base_url: "http://127.0.0.1:11434/v1".to_string(),
            model: "qwen2.5:7b".to_string(),
            api_key: None,
        }
    }

    /// Production configuration targeting OpenAI `gpt-6-luna`.
    pub fn openai_production(api_key: impl Into<String>) -> Self {
        Self {
            backend: ProviderBackend::OpenAi,
            base_url: "https://api.openai.com/v1".to_string(),
            model: "gpt-6-luna".to_string(),
            api_key: Some(api_key.into()),
        }
    }

    /// Disables cold-path LLM consolidation.
    pub fn disabled() -> Self {
        Self {
            backend: ProviderBackend::Disabled,
            base_url: String::new(),
            model: String::new(),
            api_key: None,
        }
    }

    /// Loads LLM consolidation configuration from process environment variables.
    ///
    /// Variables:
    /// - `MEM_PROVIDER`: `ollama` (default) | `openai` | `disabled`
    /// - `MEM_LLM_BASE_URL`: overrides `base_url`
    /// - `MEM_LLM_MODEL`: overrides `model`
    /// - `OPENAI_API_KEY`: sets `api_key`
    pub fn from_env() -> Self {
        Self::from_env_lookup(|key| std::env::var(key).ok())
    }

    /// Loads LLM configuration using a custom environment lookup closure.
    pub fn from_env_lookup<F>(get: F) -> Self
    where
        F: Fn(&str) -> Option<String>,
    {
        let backend = get("MEM_PROVIDER")
            .and_then(|v| ProviderBackend::parse(&v))
            .unwrap_or(ProviderBackend::Ollama);

        let mut cfg = match backend {
            ProviderBackend::Disabled => Self::disabled(),
            ProviderBackend::Ollama => Self::ollama_default(),
            ProviderBackend::OpenAi => {
                let key = get("OPENAI_API_KEY").filter(|k| !k.trim().is_empty());
                Self {
                    api_key: key,
                    ..Self::openai_production("")
                }
            }
        };

        if let Some(url) = get("MEM_LLM_BASE_URL").filter(|s| !s.trim().is_empty()) {
            cfg.base_url = url;
        }
        if let Some(model) = get("MEM_LLM_MODEL").filter(|s| !s.trim().is_empty()) {
            cfg.model = model;
        }
        if let Some(key) = get("OPENAI_API_KEY").filter(|s| !s.trim().is_empty()) {
            if cfg.backend != ProviderBackend::Disabled {
                cfg.api_key = Some(key);
            }
        }

        cfg
    }
}

/// Tuning parameters for branch-aware hybrid search and Obsidian Wiki 1-hop graph expansion.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SearchConfig {
    /// Whether to propagate relevance across 1-hop `[[Wikilink]]` outlinks, backlinks, and entities.
    pub enable_wiki_graph_expansion: bool,
    /// Decay factor applied when propagating a seed hit's score to 1-hop Wiki neighbors.
    pub graph_hop_decay: f32,
    /// Reciprocal Rank Fusion constant $k$ (typically `60.0`).
    pub rrf_k: f32,
    /// Weight of the multi-column FTS5 BM25 signal in fusion scoring.
    pub bm25_weight: f32,
    /// Weight of the vector cosine similarity signal in fusion scoring.
    pub vector_weight: f32,
    /// Weight of the 1-hop Wiki graph signal in fusion scoring.
    pub graph_weight: f32,
    /// Maximum character budget per semantic chunk when splitting Markdown sections.
    pub chunk_max_chars: usize,
    /// Number of top seed hits expanded via the Wiki link graph.
    pub seed_top_m: usize,
}

impl Default for SearchConfig {
    fn default() -> Self {
        Self {
            enable_wiki_graph_expansion: true,
            graph_hop_decay: 0.35,
            rrf_k: 60.0,
            bm25_weight: 1.0,
            vector_weight: 1.0,
            graph_weight: 0.8,
            chunk_max_chars: 800,
            seed_top_m: 5,
        }
    }
}

/// Top-level configuration bundling search, embedding, and consolidation endpoints.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct MemConfig {
    /// Hybrid search & Wiki graph parameters.
    pub search: SearchConfig,
    /// Embedding provider endpoint configuration.
    pub embedding: EmbeddingProviderConfig,
    /// Optional cold-path LLM consolidation configuration.
    pub consolidate_llm: ConsolidateLlmConfig,
}

impl MemConfig {
    /// Returns the local development preset using Ollama (`bge-m3` + `qwen2.5:7b`).
    pub fn ollama_dev() -> Self {
        Self {
            search: SearchConfig::default(),
            embedding: EmbeddingProviderConfig::ollama_default(),
            consolidate_llm: ConsolidateLlmConfig::ollama_default(),
        }
    }

    /// Returns the production preset using OpenAI (`text-embedding-3-small` + `gpt-6-luna`).
    pub fn openai_prod(api_key: impl Into<String>) -> Self {
        let key = api_key.into();
        Self {
            search: SearchConfig::default(),
            embedding: EmbeddingProviderConfig::openai_production(key.clone()),
            consolidate_llm: ConsolidateLlmConfig::openai_production(key),
        }
    }
}
