//! Branch-aware Obsidian Wiki-graph hybrid search and `/core` hot compiler for `mem`.

pub mod compiler;
pub mod config;
pub mod consolidate;
pub mod embedder;
pub mod hybrid;
pub mod ingest;
pub mod mining;
pub mod sanitize;
pub mod telemetry;
pub mod vault;
pub mod wiki;

pub use compiler::{compile_core, estimate_tokens, CompiledCoreContext};
pub use config::{
    ConsolidateLlmConfig, EmbeddingProviderConfig, MemConfig, ProviderBackend, SearchConfig,
};
pub use consolidate::{
    consolidate, consolidate_retry, note_is_valid, ConsolidateOutcome, ConsolidateStatus,
};
pub use embedder::{
    bq_hamming_distance_words, cosine_similarity, cosine_similarity_le_bytes, decode_bq_words,
    decode_f32_vec, dot_product_le_bytes, encode_bq_vec, encode_f32_vec, normalize_vec,
    post_json_http_or_curl, DeterministicHashEmbedder, EmbeddingProvider, HttpPostTransport,
    NoopEmbedder, OpenAiCompatibleEmbedder, DISABLED_MODEL,
};
pub use hybrid::{
    BranchGraph, GraphEdge, GraphNode, IndexingStats, MemSearchEngine, SearchHit, SearchRequest,
};
pub use ingest::{ingest_knowledge, IngestOutcome, IngestRequest, IngestRole};
pub use mining::{
    mark_rewind_events_mined, mine_branch, mine_commit_history, mine_rewind_pitfalls, CoChangeEdge,
    DanglingLink, MiningReport, PromotionCandidate, RewindPitfall, SuggestedLink,
};
pub use sanitize::{
    compute_simhash64, denoise_markdown, is_linkworthy_anchor, redact_secrets, simhash_hamming,
};
pub use telemetry::{
    load_branch_telemetry, record_node_read, record_search_hits, utility_multiplier, TelemetryRow,
};
pub use vault::{
    export_vault, import_vault, init_vault_template, sync_vault, VaultExportStats,
    VaultImportStats, VaultTemplateStats,
};
pub use wiki::{parse_wiki_document, EntityTriple, ParsedWikiDocument, WikiChunk, WikiLink};
