//! Branch-aware Obsidian Wiki-graph hybrid search and `/core` hot compiler for `mem`.

pub mod compiler;
pub mod config;
pub mod embedder;
pub mod hybrid;
pub mod vault;
pub mod wiki;

pub use compiler::{compile_core, estimate_tokens, CompiledCoreContext};
pub use config::{
    ConsolidateLlmConfig, EmbeddingProviderConfig, MemConfig, ProviderBackend, SearchConfig,
};
pub use embedder::{
    cosine_similarity, cosine_similarity_le_bytes, decode_f32_vec, encode_f32_vec,
    DeterministicHashEmbedder, EmbeddingProvider, HttpPostTransport, NoopEmbedder,
    OpenAiCompatibleEmbedder,
};
pub use hybrid::{IndexingStats, MemSearchEngine, SearchHit, SearchRequest};
pub use vault::{import_vault, VaultImportStats};
pub use wiki::{parse_wiki_document, EntityTriple, ParsedWikiDocument, WikiChunk, WikiLink};
