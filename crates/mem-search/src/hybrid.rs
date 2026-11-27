//! Branch-aware hybrid search combining FTS5 BM25, vectors, and Obsidian 1-hop Wiki graph.

use std::cmp::Ordering;
use std::collections::btree_map::Entry as BTreeEntry;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::hash::Hash;
use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
use std::sync::{Arc, Mutex};

use mem_core::{
    BlobId, MemError, MemoryMetadata, MemoryTier, Result, Revision, TreeId, VirtualPath,
};
use mem_store::cas::get_blob_conn;
use mem_store::workspace_list_conn;
use mem_vcs::tree::{
    apply_workspace_overlay, flatten_tree_under, parse_metadata, resolve_commit_tree_id,
    revision_reference_time_ms,
};
use mem_vcs::MemVcs;
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

use crate::compiler::{compile_core, CompiledCoreContext};
use crate::config::{ConsolidateLlmConfig, SearchConfig};
use crate::embedder::{
    bq_hamming_distance_words, cosine_similarity_le_bytes, decode_bq_words, dot_product_le_bytes,
    encode_bq_vec, encode_f32_vec, normalize_vec, EmbeddingProvider, NoopEmbedder, DISABLED_MODEL,
};
use crate::ingest::{ingest_knowledge, IngestOutcome, IngestRequest};
use crate::mining::mine_commit_history;
use crate::sanitize::compute_simhash64;
use crate::telemetry::{
    load_branch_telemetry, record_search_hits, utility_multiplier, TelemetryRow,
};
use crate::vault::{export_vault, import_vault, sync_vault, VaultExportStats, VaultImportStats};
use crate::wiki::{extract_markdown_links_from_markdown, parse_wiki_document, ParsedWikiDocument};

/// Embedding texts per request, and the chunk budget of one write transaction.
const EMBED_GROUP_CHUNKS: usize = 128;
/// Blobs indexed by another model that one search re-embeds; the rest wait for later calls.
const STALE_PER_PASS: usize = 32;
const SNAPSHOT_CACHE_CAPACITY: usize = 16;
const QUERY_VEC_CACHE_CAPACITY: usize = 256;

#[derive(Debug)]
struct LruInner<K, V> {
    cap: usize,
    map: HashMap<K, V>,
    order: VecDeque<K>,
}

#[derive(Debug)]
struct LruCache<K, V> {
    inner: Mutex<LruInner<K, V>>,
}

impl<K, V> LruCache<K, V>
where
    K: Eq + Hash + Clone,
    V: Clone,
{
    fn new(cap: usize) -> Self {
        Self {
            inner: Mutex::new(LruInner {
                cap: cap.max(1),
                map: HashMap::new(),
                order: VecDeque::new(),
            }),
        }
    }

    fn get(&self, key: &K) -> Option<V> {
        let mut guard = self.inner.lock().ok()?;
        let val = guard.map.get(key)?.clone();
        guard.order.retain(|k| k != key);
        guard.order.push_back(key.clone());
        Some(val)
    }

    fn insert(&self, key: K, val: V) {
        let Ok(mut guard) = self.inner.lock() else {
            return;
        };
        if guard.map.contains_key(&key) {
            guard.map.insert(key.clone(), val);
            guard.order.retain(|k| k != &key);
            guard.order.push_back(key);
            return;
        }
        while guard.map.len() >= guard.cap {
            let Some(evict) = guard.order.pop_front() else {
                break;
            };
            guard.map.remove(&evict);
        }
        guard.order.push_back(key.clone());
        guard.map.insert(key, val);
    }

    fn clear(&self) {
        if let Ok(mut guard) = self.inner.lock() {
            guard.map.clear();
            guard.order.clear();
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct SnapshotKey {
    root_tree_id: TreeId,
    ws_hash: [u8; 32],
}

#[derive(Clone)]
struct CachedResolvedGraph {
    outlinks_map: HashMap<VirtualPath, BTreeSet<VirtualPath>>,
    backlinks_map: HashMap<VirtualPath, BTreeSet<VirtualPath>>,
    entity_nodes: HashMap<String, BTreeSet<VirtualPath>>,
    out_degrees: HashMap<VirtualPath, usize>,
    in_degrees: HashMap<VirtualPath, usize>,
}

struct CachedGraphData {
    active_sig: u64,
    headers: HashMap<BlobId, ActiveBlobHeader>,
    links: Vec<ActiveLinkRow>,
    full_graph: CachedResolvedGraph,
}

#[derive(Clone)]
struct CachedChunkVector {
    blob_id: BlobId,
    chunk_seq: usize,
    f32_bytes: Vec<u8>,
    bq_words: Vec<u64>,
}

type CoChangeAdjacency = HashMap<VirtualPath, Vec<(VirtualPath, f32)>>;

struct CachedVectorIndex {
    active_sig: u64,
    model: String,
    global_count: u64,
    chunks: Vec<CachedChunkVector>,
}

struct BranchSearchSnapshot {
    raw_entries: Vec<(VirtualPath, BlobId, MemoryMetadata)>,
    has_any_ttl: bool,
    verified_scopes: Mutex<HashMap<(Option<String>, String), u64>>,
    cached_graph: Mutex<Option<Arc<CachedGraphData>>>,
    cached_branch_graph: Mutex<Option<(u64, Arc<BranchGraph>)>>,
    cached_vecs: Mutex<Option<Arc<CachedVectorIndex>>>,
    cached_cochanges: Mutex<Option<Arc<CoChangeAdjacency>>>,
}

impl BranchSearchSnapshot {
    fn active_entries(
        &self,
        prefix: Option<&VirtualPath>,
        ref_ms: i64,
    ) -> Vec<(VirtualPath, BlobId, MemoryMetadata)> {
        self.raw_entries
            .iter()
            .filter(|(p, _, m)| {
                (!self.has_any_ttl || !m.is_expired_at(ref_ms))
                    && prefix.is_none_or(|pref| p.starts_with(pref))
            })
            .cloned()
            .collect()
    }

    fn active_signature(&self, ref_ms: i64) -> u64 {
        if !self.has_any_ttl {
            return self.raw_entries.len() as u64;
        }
        let mut h = blake3::Hasher::new();
        let mut count = 0u64;
        for (p, b, m) in &self.raw_entries {
            if !m.is_expired_at(ref_ms) {
                count += 1;
                h.update(p.as_str().as_bytes());
                h.update(b.as_bytes());
            }
        }
        h.update(&count.to_le_bytes());
        let bytes = h.finalize();
        u64::from_le_bytes(bytes.as_bytes()[..8].try_into().unwrap_or([0u8; 8]))
    }

    fn is_scope_verified(
        &self,
        prefix: Option<&VirtualPath>,
        model: &str,
        global_count: u64,
    ) -> bool {
        let Ok(guard) = self.verified_scopes.lock() else {
            return false;
        };
        if guard.get(&(None, model.to_string())).copied() == Some(global_count) {
            return true;
        }
        if let Some(pref) = prefix {
            let key = (Some(pref.as_str().to_string()), model.to_string());
            return guard.get(&key).copied() == Some(global_count);
        }
        false
    }

    fn mark_scope_verified(&self, prefix: Option<&VirtualPath>, model: &str, global_count: u64) {
        if let Ok(mut guard) = self.verified_scopes.lock() {
            let key = (prefix.map(|p| p.as_str().to_string()), model.to_string());
            guard.insert(key, global_count);
        }
    }

    fn invalidate_graph_caches(&self) {
        if let Ok(mut g) = self.cached_graph.lock() {
            *g = None;
        }
        if let Ok(mut bg) = self.cached_branch_graph.lock() {
            *bg = None;
        }
        if let Ok(mut v) = self.cached_vecs.lock() {
            *v = None;
        }
        if let Ok(mut c) = self.cached_cochanges.lock() {
            *c = None;
        }
    }
}

/// Query parameters for branch-isolated hybrid search.
#[derive(Debug, Clone)]
pub struct SearchRequest {
    /// Target revision (`BranchWorkspace`, `BranchHead`, or `Commit`) strictly scoping results.
    pub revision: Revision,
    /// User or agent search query.
    pub query: String,
    /// Optional VFS prefix filter (e.g. `/semantic` or `/procedural`).
    pub path_prefix: Option<VirtualPath>,
    /// Maximum number of ranked documents to return.
    pub top_k: usize,
}

impl SearchRequest {
    /// Creates a search request scoped to `revision` with default `top_k = 10` and no prefix filter.
    pub fn new(revision: Revision, query: impl Into<String>) -> Self {
        Self {
            revision,
            query: query.into(),
            path_prefix: None,
            top_k: 10,
        }
    }

    /// Restricts search results to virtual paths under `prefix`.
    pub fn with_prefix(mut self, prefix: VirtualPath) -> Self {
        self.path_prefix = Some(prefix);
        self
    }

    /// Sets the maximum number of search hits to return.
    pub fn with_top_k(mut self, top_k: usize) -> Self {
        self.top_k = top_k;
        self
    }
}

/// Active note node in a branch-isolated Obsidian Wiki knowledge graph.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GraphNode {
    /// Canonical path identifier (matches `path`).
    pub id: String,
    /// Normalized virtual path in the branch.
    pub path: String,
    /// Document title extracted from frontmatter, `# H1`, or file stem.
    pub title: String,
    /// Memory tier segment (`core`, `semantic`, `procedural`, `episodic`).
    pub tier: String,
    /// Importance score `[0.0, 1.0]`.
    pub importance: f32,
    /// Normalized metadata tags attached to this node.
    pub tags: Vec<String>,
    /// Content-addressed BLAKE3 hash of the active blob.
    pub blob_id: String,
    /// Incoming resolved `[[Wikilink]]` edges inside this branch.
    pub in_degree: usize,
    /// Outgoing resolved `[[Wikilink]]` edges inside this branch.
    pub out_degree: usize,
}

/// Directed `[[Wikilink]]` edge connecting two active notes within one branch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GraphEdge {
    /// Source virtual path containing the `[[Wikilink]]`.
    pub source: String,
    /// Resolved target virtual path within the same branch.
    pub target: String,
    /// Link classification (currently `"wikilink"`).
    pub kind: String,
    /// Original reference text inside `[[...]]`.
    pub label: String,
}

/// Branch-scoped knowledge graph payload with zero cross-branch leakage.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BranchGraph {
    /// Branch name or commit hex evaluated.
    pub branch: String,
    /// Active memory nodes sorted deterministically by path.
    pub nodes: Vec<GraphNode>,
    /// Deduplicated intra-branch `[[Wikilink]]` edges sorted by `(source, target)`.
    pub edges: Vec<GraphEdge>,
}

/// Ranked memory document returned by [`MemSearchEngine::search`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SearchHit {
    /// Normalized virtual path in the target revision.
    pub path: VirtualPath,
    /// Content-addressed blob hash of the matched file.
    pub blob_id: BlobId,
    /// Document title extracted from frontmatter, `# H1`, or file stem.
    pub title: String,
    /// Heading breadcrumb path of the highest-scoring chunk.
    pub heading_path: String,
    /// Raw Markdown snippet of the matched chunk.
    pub snippet: String,
    /// 1-based start line of the snippet.
    pub start_line: usize,
    /// 1-based inclusive end line of the snippet.
    pub end_line: usize,
    /// Combined Graph-RRF + importance score.
    pub score: f32,
    /// 1-based rank in the FTS5/lexical channel, if matched directly.
    pub bm25_rank: Option<usize>,
    /// 1-based rank in the vector similarity channel, if matched directly.
    pub vector_rank: Option<usize>,
    /// 1-hop Obsidian Wiki graph propagation score contributed by linked seed hits.
    pub graph_score: f32,
    /// Active branch paths linked from this document via `[[Wikilinks]]`.
    pub outlinks: Vec<VirtualPath>,
    /// Active branch paths that link to this document via `[[Wikilinks]]`.
    pub backlinks: Vec<VirtualPath>,
}

/// Telemetry returned when indexing blobs in a revision.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexingStats {
    /// Total distinct blobs checked across the revision.
    pub blobs_checked: usize,
    /// Distinct blobs parsed and inserted into `chunk_index` / `fts_chunks` / `blob_links`.
    pub blobs_newly_indexed: usize,
    /// Distinct blobs skipped because they were already indexed by content hash.
    pub blobs_cache_hit: usize,
    /// Number of calls made to `EmbeddingProvider::embed_batch`.
    pub embedding_batches_called: usize,
    /// Total chunks written across newly indexed blobs.
    pub chunks_indexed: usize,
    /// Total graph edges written across newly indexed blobs.
    pub links_indexed: usize,
}

/// Branch-isolated search, Obsidian Wiki graph indexer, and `/core` hot compiler.
#[derive(Clone)]
pub struct MemSearchEngine {
    vcs: MemVcs,
    embedder: Arc<dyn EmbeddingProvider>,
    config: SearchConfig,
    total_embedding_calls: Arc<AtomicUsize>,
    snapshot_cache: Arc<LruCache<SnapshotKey, Arc<BranchSearchSnapshot>>>,
    query_vec_cache: Arc<LruCache<(String, String), Vec<f32>>>,
}

impl std::fmt::Debug for MemSearchEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemSearchEngine")
            .field("embedder", &self.embedder.provider_name())
            .field("config", &self.config)
            .field("total_embedding_calls", &self.total_embedding_calls())
            .finish()
    }
}

impl MemSearchEngine {
    /// Creates a Level-1 zero-model search engine (FTS5 BM25 + Obsidian Wiki graph).
    pub fn new_bm25_only(vcs: MemVcs) -> Self {
        Self::new(vcs, Arc::new(NoopEmbedder), SearchConfig::default())
    }

    /// Creates a hybrid search engine with a custom [`EmbeddingProvider`] and [`SearchConfig`].
    pub fn new(vcs: MemVcs, embedder: Arc<dyn EmbeddingProvider>, config: SearchConfig) -> Self {
        Self {
            vcs,
            embedder,
            config,
            total_embedding_calls: Arc::new(AtomicUsize::new(0)),
            snapshot_cache: Arc::new(LruCache::new(SNAPSHOT_CACHE_CAPACITY)),
            query_vec_cache: Arc::new(LruCache::new(QUERY_VEC_CACHE_CAPACITY)),
        }
    }

    /// Returns a reference to the underlying [`MemVcs`] engine.
    pub fn vcs(&self) -> &MemVcs {
        &self.vcs
    }

    /// Returns the active [`SearchConfig`].
    pub fn config(&self) -> &SearchConfig {
        &self.config
    }

    /// Returns the active [`EmbeddingProvider`].
    pub fn embedder(&self) -> &Arc<dyn EmbeddingProvider> {
        &self.embedder
    }

    /// Returns total calls made to `embed_batch` across the lifetime of this engine instance.
    pub fn total_embedding_calls(&self) -> usize {
        self.total_embedding_calls.load(AtomicOrdering::Relaxed)
    }

    /// Ensures every distinct `blob_id` visible in `rev` (and optional `prefix`) is indexed once.
    pub fn ensure_revision_indexed(
        &self,
        rev: &Revision,
        prefix: Option<&VirtualPath>,
    ) -> Result<IndexingStats> {
        let active = self.vcs.list(rev, prefix)?;
        let pairs: Vec<(VirtualPath, BlobId)> =
            active.into_iter().map(|(p, b, _)| (p, b)).collect();
        self.index_unique_blobs(&pairs)
    }

    /// Ensures a single `(path, blob_id)` pair is indexed if `blob_id` is not yet in `chunk_index`.
    pub fn index_blob_if_needed(&self, path: &VirtualPath, blob_id: &BlobId) -> Result<bool> {
        let stats = self.index_unique_blobs(&[(path.clone(), *blob_id)])?;
        Ok(stats.blobs_newly_indexed > 0)
    }

    /// Indexes all uncached distinct blobs from `items`, computing embeddings outside the DB lock.
    /// Blobs indexed by another model are re-embedded, at most [`STALE_PER_PASS`] per call, so a
    /// model change never stalls one search; later calls catch up.
    pub fn index_unique_blobs(&self, items: &[(VirtualPath, BlobId)]) -> Result<IndexingStats> {
        self.index_blobs(items, false)
    }

    /// `force` re-indexes every blob in `items`, fresh or not, with no per-call limit.
    fn index_blobs(&self, items: &[(VirtualPath, BlobId)], force: bool) -> Result<IndexingStats> {
        let mut unique: BTreeMap<BlobId, VirtualPath> = BTreeMap::new();
        for (path, blob_id) in items {
            unique.entry(*blob_id).or_insert_with(|| path.clone());
        }

        let mut stats = IndexingStats {
            blobs_checked: unique.len(),
            ..IndexingStats::default()
        };
        if unique.is_empty() {
            return Ok(stats);
        }

        // Step 1: Under short read lock, find blobs that are missing or stale (another model)
        // and read their raw bytes.
        let model = self.embedder.model_id();
        let mut missing_raw: Vec<(BlobId, VirtualPath, Vec<u8>)> = Vec::new();
        let mut stale_taken = 0usize;
        self.vcs.store().with_conn(|conn| {
            let mut exists_stmt = conn
                .prepare_cached("SELECT 1 FROM chunk_index WHERE blob_id = ?1 LIMIT 1")
                .map_err(|e| MemError::Storage(format!("failed to prepare index check: {e}")))?;

            for (&blob_id, rep_path) in &unique {
                let already_indexed = exists_stmt
                    .exists(params![blob_id.to_hex()])
                    .map_err(|e| MemError::Storage(format!("failed to check chunk_index: {e}")))?;
                if already_indexed && !force {
                    if index_is_fresh(conn, &blob_id.to_hex(), &model)? {
                        stats.blobs_cache_hit += 1;
                        continue;
                    }
                    if stale_taken == STALE_PER_PASS {
                        continue; // re-embedded by a later call; not a cache hit
                    }
                    stale_taken += 1;
                }

                let Some((bytes, _ct)) = get_blob_conn(conn, &blob_id)? else {
                    return Err(MemError::BlobNotFound(blob_id.to_hex()));
                };
                missing_raw.push((blob_id, rep_path.clone(), bytes));
            }
            Ok(())
        })?;

        if missing_raw.is_empty() {
            return Ok(stats);
        }

        // Step 2: Parse outside the SQLite lock, then embed and write in groups of about
        // EMBED_GROUP_CHUNKS chunks: a slow or failed call never touches rows of other groups.
        let mut group: Vec<(BlobId, ParsedWikiDocument)> = Vec::new();
        let mut group_chunks = 0usize;
        for (blob_id, path, bytes) in missing_raw {
            let Ok(text) = std::str::from_utf8(&bytes) else {
                // Skip non-UTF-8 binary blobs gracefully.
                continue;
            };
            let parsed = parse_wiki_document(&path, text, self.config.chunk_max_chars);
            if !group.is_empty() && group_chunks + parsed.chunks.len() > EMBED_GROUP_CHUNKS {
                self.write_group(std::mem::take(&mut group), &model, force, &mut stats)?;
                group_chunks = 0;
            }
            group_chunks += parsed.chunks.len();
            group.push((blob_id, parsed));
        }
        if !group.is_empty() {
            self.write_group(group, &model, force, &mut stats)?;
        }
        Ok(stats)
    }

    /// Embeds one group (requests of at most EMBED_GROUP_CHUNKS texts) and writes its chunk,
    /// full-text, link, and model rows in one transaction.
    fn write_group(
        &self,
        parsed_blobs: Vec<(BlobId, ParsedWikiDocument)>,
        model: &str,
        force: bool,
        stats: &mut IndexingStats,
    ) -> Result<()> {
        struct PendingBlob {
            blob_id: BlobId,
            parsed: ParsedWikiDocument,
            embeddings: Option<Vec<Vec<f32>>>,
        }
        let chunk_counts: Vec<usize> = parsed_blobs.iter().map(|(_, p)| p.chunks.len()).collect();
        let all_chunk_texts: Vec<String> = parsed_blobs
            .iter()
            .flat_map(|(_, p)| p.chunks.iter().map(|c| c.chunk_text.clone()))
            .collect();
        let mut flat_embeddings: Option<Vec<Vec<f32>>> =
            (!all_chunk_texts.is_empty()).then(|| Vec::with_capacity(all_chunk_texts.len()));
        for texts in all_chunk_texts.chunks(EMBED_GROUP_CHUNKS) {
            self.total_embedding_calls
                .fetch_add(1, AtomicOrdering::Relaxed);
            stats.embedding_batches_called += 1;
            match self.embedder.embed_batch(texts)? {
                Some(vecs) => flat_embeddings.get_or_insert_with(Vec::new).extend(vecs),
                None => {
                    flat_embeddings = None;
                    break;
                }
            }
        }

        let mut pending: Vec<PendingBlob> = Vec::with_capacity(parsed_blobs.len());
        let mut vec_cursor = 0usize;
        for ((blob_id, parsed), count) in parsed_blobs.into_iter().zip(chunk_counts) {
            let blob_vecs = flat_embeddings.as_ref().map(|all| {
                let slice = &all[vec_cursor..vec_cursor + count];
                vec_cursor += count;
                slice.to_vec()
            });
            pending.push(PendingBlob {
                blob_id,
                parsed,
                embeddings: blob_vecs,
            });
        }

        // Step 3: Atomically persist chunks, FTS5 rows, and Wiki graph links in one transaction.
        self.vcs.store().with_tx(|tx| {
            let mut exists_stmt = tx
                .prepare_cached("SELECT 1 FROM chunk_index WHERE blob_id = ?1 LIMIT 1")
                .map_err(|e| MemError::Storage(format!("failed to prepare tx index check: {e}")))?;
            let mut chunk_stmt = tx
                .prepare_cached(
                    "INSERT OR IGNORE INTO chunk_index
                     (blob_id, chunk_seq, title, aliases, tags, heading_path, chunk_text, raw_snippet, start_line, end_line, embedding, embedding_bq)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
                )
                .map_err(|e| MemError::Storage(format!("failed to prepare chunk insert: {e}")))?;
            let mut fts_stmt = tx
                .prepare_cached(
                    "INSERT INTO fts_chunks
                     (blob_id, chunk_seq, title, aliases, tags, heading_path, chunk_text)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                )
                .map_err(|e| MemError::Storage(format!("failed to prepare fts insert: {e}")))?;
            let mut link_stmt = tx
                .prepare_cached(
                    "INSERT OR IGNORE INTO blob_links
                     (source_blob_id, link_kind, source_entity, relation, target_ref)
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                )
                .map_err(|e| MemError::Storage(format!("failed to prepare link insert: {e}")))?;
            let mut sig_stmt = tx
                .prepare_cached(
                    "INSERT OR REPLACE INTO blob_signatures (blob_id, simhash64, token_count)
                     VALUES (?1, ?2, ?3)",
                )
                .map_err(|e| MemError::Storage(format!("failed to prepare signature insert: {e}")))?;

            for item in &pending {
                let hex = item.blob_id.to_hex();
                let already = exists_stmt
                    .exists(params![hex])
                    .map_err(|e| MemError::Storage(format!("failed tx existence check: {e}")))?;
                if already {
                    if !force && index_is_fresh(tx, &hex, model)? {
                        continue;
                    }
                    // Vectors from another model: drop this blob's rows, then insert them again.
                    for sql in [
                        "DELETE FROM chunk_index WHERE blob_id = ?1",
                        "DELETE FROM fts_chunks WHERE blob_id = ?1",
                        "DELETE FROM blob_links WHERE source_blob_id = ?1",
                    ] {
                        tx.execute(sql, params![hex]).map_err(|e| {
                            MemError::Storage(format!("failed to drop stale index rows: {e}"))
                        })?;
                    }
                }

                let aliases_joined = item.parsed.aliases.join(" | ");
                let tags_joined = item.parsed.tags.join(" ");
                let fts_title = augment_fts_text(&item.parsed.title);
                let fts_aliases = augment_fts_text(&aliases_joined);
                let fts_tags = augment_fts_text(&tags_joined);

                for (idx, chunk) in item.parsed.chunks.iter().enumerate() {
                    let (emb_bytes, bq_bytes) = match item
                        .embeddings
                        .as_ref()
                        .and_then(|vecs| vecs.get(idx))
                    {
                        Some(v) => {
                            let norm = normalize_vec(v);
                            (Some(encode_f32_vec(&norm)), Some(encode_bq_vec(&norm)))
                        }
                        None => (None, None),
                    };

                    let seq_i64 = i64::try_from(chunk.chunk_seq).unwrap_or(0);
                    let start_i64 = i64::try_from(chunk.start_line).unwrap_or(1);
                    let end_i64 = i64::try_from(chunk.end_line).unwrap_or(1);

                    chunk_stmt
                        .execute(params![
                            hex,
                            seq_i64,
                            item.parsed.title,
                            aliases_joined,
                            tags_joined,
                            chunk.heading_path,
                            chunk.chunk_text,
                            chunk.raw_snippet,
                            start_i64,
                            end_i64,
                            emb_bytes,
                            bq_bytes,
                        ])
                        .map_err(|e| {
                            MemError::Storage(format!("failed to insert chunk_index: {e}"))
                        })?;

                    let fts_heading = augment_fts_text(&chunk.heading_path);
                    let fts_body = augment_fts_text(&chunk.raw_snippet);

                    fts_stmt
                        .execute(params![
                            hex,
                            seq_i64,
                            fts_title,
                            fts_aliases,
                            fts_tags,
                            fts_heading,
                            fts_body,
                        ])
                        .map_err(|e| {
                            MemError::Storage(format!("failed to insert fts_chunks: {e}"))
                        })?;

                    stats.chunks_indexed += 1;
                }

                for link in &item.parsed.wikilinks {
                    link_stmt
                        .execute(params![
                            hex,
                            "wikilink",
                            item.parsed.title,
                            "references",
                            link.target,
                        ])
                        .map_err(|e| {
                            MemError::Storage(format!("failed to insert wikilink: {e}"))
                        })?;
                    stats.links_indexed += 1;
                }

                for tag in &item.parsed.tags {
                    link_stmt
                        .execute(params![
                            hex,
                            "tag",
                            item.parsed.title,
                            "tagged",
                            tag,
                        ])
                        .map_err(|e| MemError::Storage(format!("failed to insert tag link: {e}")))?;
                    stats.links_indexed += 1;
                }

                for ent in &item.parsed.entities {
                    link_stmt
                        .execute(params![
                            hex,
                            "entity_rel",
                            ent.source,
                            ent.relation,
                            ent.target,
                        ])
                        .map_err(|e| {
                            MemError::Storage(format!("failed to insert entity link: {e}"))
                        })?;
                    stats.links_indexed += 1;
                }

                let body_for_sig: String = item
                    .parsed
                    .chunks
                    .iter()
                    .map(|c| c.raw_snippet.as_str())
                    .collect::<Vec<_>>()
                    .join("\n");
                let (simhash, tok_count) = compute_simhash64(&body_for_sig);
                let simhash_i64 = i64::from_ne_bytes(simhash.to_ne_bytes());
                sig_stmt
                    .execute(params![hex, simhash_i64, i64::try_from(tok_count).unwrap_or(0)])
                    .map_err(|e| {
                        MemError::Storage(format!("failed to insert blob_signatures: {e}"))
                    })?;

                tx.execute(
                    "INSERT OR REPLACE INTO blob_index_model (blob_id, model) VALUES (?1, ?2)",
                    params![hex, model],
                )
                .map_err(|e| MemError::Storage(format!("failed to record index model: {e}")))?;
                stats.blobs_newly_indexed += 1;
            }

            Ok(())
        })
    }

    /// `model_id` of the active embedder; [`DISABLED_MODEL`] means BM25-only search.
    pub fn embedding_model(&self) -> String {
        self.embedder.model_id()
    }

    /// Index coverage over all indexed blobs: `(files, files_with_vectors, chunks)`.
    pub fn index_stats(&self) -> Result<(u64, u64, u64)> {
        self.vcs.store().with_conn(|conn| {
            conn.query_row(
                "SELECT count(DISTINCT blob_id),
                        count(DISTINCT CASE WHEN embedding IS NOT NULL THEN blob_id END),
                        count(*)
                 FROM chunk_index",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .map_err(|e| MemError::Storage(format!("failed to read index stats: {e}")))
        })
    }

    /// Indexes every branch view (head plus staged changes) again with the current embedder.
    /// Blobs only in older commits keep their rows.
    pub fn reindex_all(&self) -> Result<IndexingStats> {
        let (_, with_vectors, _) = self.index_stats()?;
        if self.embedder.model_id() == DISABLED_MODEL && with_vectors > 0 {
            return Err(MemError::Storage(
                "no embedding model is active: refusing to rebuild an index that holds vectors \
                 (start the embedding provider, then rebuild)"
                    .into(),
            ));
        }
        let mut items = Vec::new();
        for (branch, _) in self.vcs.store().list_refs()? {
            let view = self.vcs.list(&Revision::BranchWorkspace(branch), None)?;
            items.extend(view.into_iter().map(|(path, blob_id, _)| (path, blob_id)));
        }
        // Rows are replaced blob by blob, group by group: a failure keeps the old rows of the rest.
        let stats = self.index_blobs(&items, true)?;
        self.snapshot_cache.clear();
        Ok(stats)
    }

    fn load_or_build_snapshot(&self, rev: &Revision) -> Result<(Arc<BranchSearchSnapshot>, i64)> {
        self.vcs.store().with_conn(|conn| {
            let root_tree_id = resolve_commit_tree_id(conn, rev)?;
            let ref_ms = revision_reference_time_ms(conn, rev)?;
            let (ws_entries, ws_hash) = match rev {
                Revision::BranchWorkspace(branch) => {
                    let entries = workspace_list_conn(conn, branch)?;
                    let hash = if entries.is_empty() {
                        [0u8; 32]
                    } else {
                        let mut hasher = blake3::Hasher::new();
                        for e in &entries {
                            hasher.update(e.path.as_str().as_bytes());
                            hasher.update(e.op.as_str().as_bytes());
                            if let Some(b) = &e.blob_id {
                                hasher.update(b.as_bytes());
                            }
                            if let Some(m) = &e.meta_json {
                                hasher.update(m.as_bytes());
                            }
                        }
                        *hasher.finalize().as_bytes()
                    };
                    (entries, hash)
                }
                Revision::BranchHead(_) | Revision::Commit(_) => (Vec::new(), [0u8; 32]),
            };

            let key = SnapshotKey {
                root_tree_id,
                ws_hash,
            };
            if let Some(cached) = self.snapshot_cache.get(&key) {
                return Ok((cached, ref_ms));
            }

            let mut flat = flatten_tree_under(conn, &root_tree_id, None)?;
            if !ws_entries.is_empty() {
                apply_workspace_overlay(&mut flat, &ws_entries)?;
            }

            let mut raw_entries = Vec::with_capacity(flat.len());
            let mut has_any_ttl = false;
            for (path, (blob_id, meta_json)) in flat {
                let meta = parse_metadata(&meta_json)?;
                if meta.ttl_seconds.is_some() {
                    has_any_ttl = true;
                }
                raw_entries.push((path, blob_id, meta));
            }

            let snapshot = Arc::new(BranchSearchSnapshot {
                raw_entries,
                has_any_ttl,
                verified_scopes: Mutex::new(HashMap::new()),
                cached_graph: Mutex::new(None),
                cached_branch_graph: Mutex::new(None),
                cached_vecs: Mutex::new(None),
                cached_cochanges: Mutex::new(None),
            });
            self.snapshot_cache.insert(key, Arc::clone(&snapshot));
            Ok((snapshot, ref_ms))
        })
    }

    fn ensure_snapshot_indexed(
        &self,
        snap: &BranchSearchSnapshot,
        prefix: Option<&VirtualPath>,
        active_entries: &[(VirtualPath, BlobId, MemoryMetadata)],
    ) -> Result<u64> {
        let model = self.embedder.model_id();
        let global_count = self
            .vcs
            .store()
            .with_conn(|conn| current_model_index_count(conn, &model))?;

        if snap.is_scope_verified(prefix, &model, global_count) {
            return Ok(global_count);
        }

        let pairs: Vec<(VirtualPath, BlobId)> = active_entries
            .iter()
            .map(|(p, b, _)| (p.clone(), *b))
            .collect();
        let stats = self.index_unique_blobs(&pairs)?;
        if stats.blobs_newly_indexed > 0 {
            snap.invalidate_graph_caches();
        }
        let updated_count = if stats.blobs_newly_indexed > 0 {
            self.vcs
                .store()
                .with_conn(|conn| current_model_index_count(conn, &model))?
        } else {
            global_count
        };
        if stats.blobs_newly_indexed + stats.blobs_cache_hit == stats.blobs_checked {
            snap.mark_scope_verified(prefix, &model, updated_count);
        }
        Ok(updated_count)
    }

    /// Executes branch-scoped hybrid search: BM25 + Vector + 1-hop Wiki Graph RRF fusion.
    pub fn search(&self, req: &SearchRequest) -> Result<Vec<SearchHit>> {
        if req.top_k == 0 || req.query.trim().is_empty() {
            return Ok(Vec::new());
        }

        // 1. Resolve strictly active files for the target revision via immutable snapshot cache.
        let (snap, ref_ms) = self.load_or_build_snapshot(&req.revision)?;
        let active_entries = snap.active_entries(req.path_prefix.as_ref(), ref_ms);
        if active_entries.is_empty() {
            return Ok(Vec::new());
        }
        let global_count =
            self.ensure_snapshot_indexed(&snap, req.path_prefix.as_ref(), &active_entries)?;

        // Group active paths by BlobId and preserve metadata per path.
        let mut paths_by_blob: HashMap<BlobId, Vec<VirtualPath>> = HashMap::new();
        let mut meta_by_path: HashMap<VirtualPath, (BlobId, MemoryMetadata)> = HashMap::new();
        for (path, blob_id, metadata) in active_entries {
            paths_by_blob.entry(blob_id).or_default().push(path.clone());
            meta_by_path.insert(path, (blob_id, metadata));
        }

        // 2. Embed query outside the SQLite lock (reusing Query Embedding LRU cache).
        let model = self.embedder.model_id();
        let query_vec = if model == DISABLED_MODEL {
            None
        } else {
            let cache_key = (model.clone(), req.query.trim().to_string());
            if let Some(cached) = self.query_vec_cache.get(&cache_key) {
                Some(cached)
            } else {
                let computed = self.embedder.embed_query(&req.query)?;
                if let Some(ref vec) = computed {
                    self.query_vec_cache.insert(cache_key, vec.clone());
                }
                computed
            }
        };

        // 3. Reuse cached headers/links/graph and active vector index when available.
        let fts_limit = (req.top_k * 5).max(50);
        let active_sig = snap.active_signature(ref_ms);
        let cached_graph = snap.cached_graph.lock().ok().and_then(|g| g.clone());
        let reusable_graph = cached_graph.filter(|g| g.active_sig == active_sig);
        let need_graph_load = reusable_graph.is_none();

        let cached_vecs = if query_vec.is_some() {
            snap.cached_vecs
                .lock()
                .ok()
                .and_then(|v| v.clone())
                .filter(|v| {
                    v.active_sig == active_sig && v.model == model && v.global_count == global_count
                })
        } else {
            None
        };
        let need_vec_load = query_vec.is_some() && cached_vecs.is_none();
        let branch_for_telemetry = match &req.revision {
            Revision::BranchWorkspace(b) | Revision::BranchHead(b) => Some(b.as_str()),
            Revision::Commit(_) => None,
        };

        let mut active_data = self.vcs.store().with_conn(|conn| {
            load_active_search_data(
                conn,
                &paths_by_blob,
                &req.query,
                need_vec_load.then_some(model.as_str()),
                fts_limit,
                need_graph_load,
                branch_for_telemetry,
            )
        })?;

        if let Some(q_raw) = query_vec.as_deref() {
            let vec_slice: &[CachedChunkVector] = if let Some(ref cached) = cached_vecs {
                &cached.chunks
            } else {
                let loaded = active_data.loaded_vecs.take().unwrap_or_default();
                if req.path_prefix.is_none() && !snap.has_any_ttl {
                    let cached = Arc::new(CachedVectorIndex {
                        active_sig,
                        model: model.clone(),
                        global_count,
                        chunks: loaded,
                    });
                    if let Ok(mut guard) = snap.cached_vecs.lock() {
                        *guard = Some(Arc::clone(&cached));
                    }
                    // Score from the newly cached Arc.
                    let (vh, vch) = score_chunk_vectors(
                        q_raw,
                        &cached.chunks,
                        Some(&paths_by_blob),
                        fts_limit,
                    )?;
                    active_data.vec_hits = vh;
                    active_data.vec_chunk_hits = vch;
                    &[]
                } else {
                    let (vh, vch) = score_chunk_vectors(q_raw, &loaded, None, fts_limit)?;
                    active_data.vec_hits = vh;
                    active_data.vec_chunk_hits = vch;
                    &[]
                }
            };
            if !vec_slice.is_empty() {
                let filter = if req.path_prefix.is_some() || snap.has_any_ttl {
                    Some(&paths_by_blob)
                } else {
                    None
                };
                let (vh, vch) = score_chunk_vectors(q_raw, vec_slice, filter, fts_limit)?;
                active_data.vec_hits = vh;
                active_data.vec_chunk_hits = vch;
            }
        }

        let (headers_map, links_slice, resolved_graph) = if let Some(cached) = reusable_graph {
            if req.path_prefix.is_none() && !snap.has_any_ttl {
                (
                    cached.headers.clone(),
                    cached.links.clone(),
                    cached.full_graph.clone(),
                )
            } else {
                let scoped = build_resolved_graph(&paths_by_blob, &cached.headers, &cached.links);
                (cached.headers.clone(), cached.links.clone(), scoped)
            }
        } else {
            let headers_map = active_data.headers;
            let links_vec = active_data.links;
            let resolved = build_resolved_graph(&paths_by_blob, &headers_map, &links_vec);
            if req.path_prefix.is_none() && !snap.has_any_ttl {
                let cached = Arc::new(CachedGraphData {
                    active_sig,
                    headers: headers_map.clone(),
                    links: links_vec.clone(),
                    full_graph: resolved.clone(),
                });
                if let Ok(mut guard) = snap.cached_graph.lock() {
                    *guard = Some(cached);
                }
            }
            (headers_map, links_vec, resolved)
        };

        let title_by_blob: HashMap<BlobId, String> = headers_map
            .iter()
            .map(|(b, h)| (*b, h.title.clone()))
            .collect();
        let CachedResolvedGraph {
            outlinks_map,
            backlinks_map,
            entity_nodes,
            out_degrees,
            in_degrees,
        } = resolved_graph;

        // 6. Rank Channel 1 (FTS5 + CJK/lexical fallback) at both chunk and document levels.
        let mut bm25_chunk_rank: HashMap<(BlobId, usize), usize> = HashMap::new();
        let mut bm25_sorted_chunks = active_data.fts_hits.clone();
        bm25_sorted_chunks.sort_by(|a, b| a.score.partial_cmp(&b.score).unwrap_or(Ordering::Equal));
        for (idx, hit) in bm25_sorted_chunks.iter().enumerate() {
            bm25_chunk_rank
                .entry((hit.blob_id, hit.chunk_seq))
                .or_insert(idx + 1);
        }

        let mut bm25_best_by_blob: HashMap<BlobId, (usize, f64)> = HashMap::new();
        for hit in &active_data.fts_hits {
            bm25_best_by_blob
                .entry(hit.blob_id)
                .and_modify(|cur| {
                    if hit.score < cur.1 {
                        *cur = (hit.chunk_seq, hit.score);
                    }
                })
                .or_insert((hit.chunk_seq, hit.score));
        }

        let mut bm25_ranked_blobs: Vec<(BlobId, usize, f64)> = bm25_best_by_blob
            .into_iter()
            .map(|(b, (seq, s))| (b, seq, s))
            .collect();
        bm25_ranked_blobs.sort_by(|a, b| a.2.partial_cmp(&b.2).unwrap_or(Ordering::Equal));

        let mut bm25_rank_by_path: HashMap<VirtualPath, (usize, usize)> = HashMap::new();
        let mut rank_counter = 1usize;
        for (blob_id, chunk_seq, _) in &bm25_ranked_blobs {
            if let Some(paths) = paths_by_blob.get(blob_id) {
                for path in paths {
                    bm25_rank_by_path
                        .entry(path.clone())
                        .or_insert((rank_counter, *chunk_seq));
                    rank_counter += 1;
                }
            }
        }

        // Supplement with exact lexical fallback only when FTS5 produced no matches.
        if bm25_rank_by_path.is_empty() && !active_data.lexical_fallback.is_empty() {
            for (idx, (blob_id, chunk_seq)) in active_data.lexical_fallback.iter().enumerate() {
                bm25_chunk_rank
                    .entry((*blob_id, *chunk_seq))
                    .or_insert(idx + 1);
                if let Some(paths) = paths_by_blob.get(blob_id) {
                    for path in paths {
                        if !bm25_rank_by_path.contains_key(path) {
                            bm25_rank_by_path.insert(path.clone(), (rank_counter, *chunk_seq));
                            rank_counter += 1;
                        }
                    }
                }
            }
        }

        // 7. Rank Channel 2 (Vector Cosine Similarity) at both chunk and document levels.
        let mut vec_chunk_rank: HashMap<(BlobId, usize), usize> = HashMap::new();
        for (idx, (blob_id, chunk_seq, _)) in active_data.vec_chunk_hits.iter().enumerate() {
            vec_chunk_rank
                .entry((*blob_id, *chunk_seq))
                .or_insert(idx + 1);
        }

        let mut vec_rank_by_path: HashMap<VirtualPath, (usize, usize)> = HashMap::new();
        if !active_data.vec_hits.is_empty() {
            let mut vrank = 1usize;
            for (blob_id, chunk_seq, _) in &active_data.vec_hits {
                if let Some(paths) = paths_by_blob.get(blob_id) {
                    for path in paths {
                        vec_rank_by_path
                            .entry(path.clone())
                            .or_insert((vrank, *chunk_seq));
                        vrank += 1;
                    }
                }
            }
        }

        // 8. Compute direct seed RRF scores before graph expansion.
        let mut candidate_paths: BTreeSet<VirtualPath> = BTreeSet::new();
        for p in bm25_rank_by_path.keys() {
            candidate_paths.insert(p.clone());
        }
        for p in vec_rank_by_path.keys() {
            candidate_paths.insert(p.clone());
        }

        let k = self.config.rrf_k.max(1.0);
        let mut seed_scores: Vec<(VirtualPath, f32)> = Vec::with_capacity(candidate_paths.len());
        for path in &candidate_paths {
            let bm25_part = bm25_rank_by_path
                .get(path)
                .map(|(r, _)| self.config.bm25_weight / (k + *r as f32))
                .unwrap_or(0.0);
            let vec_part = vec_rank_by_path
                .get(path)
                .map(|(r, _)| self.config.vector_weight / (k + *r as f32))
                .unwrap_or(0.0);
            seed_scores.push((path.clone(), bm25_part + vec_part));
        }
        seed_scores.sort_by(|a, b| {
            b.1.partial_cmp(&a.1)
                .unwrap_or(Ordering::Equal)
                .then_with(|| a.0.cmp(&b.0))
        });

        // 9. Channel 3: Hub-Penalized Obsidian 1-Hop Wiki Graph + Co-Change Expansion.
        let cochange_map: Option<Arc<CoChangeAdjacency>> =
            if self.config.enable_wiki_graph_expansion {
                if let Some(branch) = branch_for_telemetry {
                    let cached = snap.cached_cochanges.lock().ok().and_then(|c| c.clone());
                    if let Some(cached) = cached {
                        Some(cached)
                    } else if let Ok((edges, _)) = mine_commit_history(&self.vcs, branch, 100) {
                        let mut adj: CoChangeAdjacency = HashMap::new();
                        for edge in edges {
                            adj.entry(edge.path_a.clone())
                                .or_default()
                                .push((edge.path_b.clone(), edge.weight));
                            adj.entry(edge.path_b)
                                .or_default()
                                .push((edge.path_a, edge.weight));
                        }
                        let arc_adj = Arc::new(adj);
                        if let Ok(mut guard) = snap.cached_cochanges.lock() {
                            *guard = Some(Arc::clone(&arc_adj));
                        }
                        Some(arc_adj)
                    } else {
                        None
                    }
                } else {
                    None
                }
            } else {
                None
            };

        let mut graph_scores: HashMap<VirtualPath, f32> = HashMap::new();
        if self.config.enable_wiki_graph_expansion {
            let seed_limit = self.config.seed_top_m.max(1);
            for (seed_path, seed_score) in seed_scores.iter().take(seed_limit) {
                let hop_delta = *seed_score * self.config.graph_hop_decay;
                if hop_delta <= 0.0 {
                    continue;
                }
                let seed_out_deg = out_degrees.get(seed_path).copied().unwrap_or(1).max(1);
                let seed_in_deg = in_degrees.get(seed_path).copied().unwrap_or(1).max(1);

                if let Some(outs) = outlinks_map.get(seed_path) {
                    for target in outs {
                        if target != seed_path {
                            let target_in_deg = in_degrees.get(target).copied().unwrap_or(1).max(1);
                            let deg_factor = ((seed_out_deg * target_in_deg) as f32).sqrt();
                            *graph_scores.entry(target.clone()).or_insert(0.0) +=
                                hop_delta / deg_factor;
                            candidate_paths.insert(target.clone());
                        }
                    }
                }
                if let Some(backs) = backlinks_map.get(seed_path) {
                    for source in backs {
                        if source != seed_path {
                            let source_out_deg =
                                out_degrees.get(source).copied().unwrap_or(1).max(1);
                            let deg_factor = ((source_out_deg * seed_in_deg) as f32).sqrt();
                            *graph_scores.entry(source.clone()).or_insert(0.0) +=
                                hop_delta / deg_factor;
                            candidate_paths.insert(source.clone());
                        }
                    }
                }
                if let Some(co_neighbors) = cochange_map.as_ref().and_then(|m| m.get(seed_path)) {
                    for (neighbor, weight) in co_neighbors {
                        if neighbor != seed_path && meta_by_path.contains_key(neighbor) {
                            *graph_scores.entry(neighbor.clone()).or_insert(0.0) +=
                                hop_delta * 0.4 * weight;
                            candidate_paths.insert(neighbor.clone());
                        }
                    }
                }
                // Entity-triple 1-hop propagation at 0.7x wikilink decay.
                if let Some((seed_blob, _)) = meta_by_path.get(seed_path) {
                    for link in &links_slice {
                        if link.source_blob_id == *seed_blob && link.link_kind == "entity_rel" {
                            for entity_key in [
                                normalize_lookup_key(&link.target_ref),
                                normalize_lookup_key(&link.source_entity),
                            ] {
                                if let Some(neighbors) = entity_nodes.get(&entity_key) {
                                    let ent_deg = (neighbors.len().max(1) as f32).sqrt();
                                    for neighbor in neighbors {
                                        if neighbor != seed_path {
                                            *graph_scores.entry(neighbor.clone()).or_insert(0.0) +=
                                                (hop_delta * 0.7) / ent_deg;
                                            candidate_paths.insert(neighbor.clone());
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        if candidate_paths.is_empty() {
            return Ok(Vec::new());
        }

        // 10. Combine scores with chunk-level RRF best-chunk selection, importance, and tier prior.
        struct ScoredCandidate {
            path: VirtualPath,
            blob_id: BlobId,
            chunk_seq: usize,
            score: f32,
            bm25_rank: Option<usize>,
            vector_rank: Option<usize>,
            graph_score: f32,
        }

        let mut chunks_by_blob: HashMap<BlobId, HashSet<usize>> = HashMap::new();
        for &(blob_id, seq) in bm25_chunk_rank.keys().chain(vec_chunk_rank.keys()) {
            chunks_by_blob.entry(blob_id).or_default().insert(seq);
        }

        let mut scored: Vec<ScoredCandidate> = Vec::with_capacity(candidate_paths.len());
        for path in candidate_paths {
            let Some((blob_id, metadata)) = meta_by_path.get(&path) else {
                continue;
            };
            let bm25_entry = bm25_rank_by_path.get(&path).copied();
            let vec_entry = vec_rank_by_path.get(&path).copied();
            let g_score = graph_scores.get(&path).copied().unwrap_or(0.0);

            let bm25_part = bm25_entry
                .map(|(r, _)| self.config.bm25_weight / (k + r as f32))
                .unwrap_or(0.0);
            let vec_part = vec_entry
                .map(|(r, _)| self.config.vector_weight / (k + r as f32))
                .unwrap_or(0.0);
            let graph_part = self.config.graph_weight * g_score;

            let importance_boost = 1.0 + 0.2 * metadata.importance.clamp(0.0, 1.0);
            let tier_prior = if req.path_prefix.is_none() && path.tier() == MemoryTier::Episodic {
                0.92
            } else {
                1.0
            };
            let utility_boost = active_data
                .telemetry
                .get(&path)
                .map(|row| utility_multiplier(row, blob_id))
                .unwrap_or(1.0);
            let total_score =
                (bm25_part + vec_part + graph_part) * importance_boost * tier_prior * utility_boost;

            let best_chunk_seq = if let Some(seqs) = chunks_by_blob.get(blob_id) {
                seqs.iter()
                    .copied()
                    .max_by(|&seq_a, &seq_b| {
                        let score_chunk = |seq: usize| -> f32 {
                            let br = bm25_chunk_rank
                                .get(&(*blob_id, seq))
                                .map(|&r| self.config.bm25_weight / (k + r as f32))
                                .unwrap_or(0.0);
                            let vr = vec_chunk_rank
                                .get(&(*blob_id, seq))
                                .map(|&r| self.config.vector_weight / (k + r as f32))
                                .unwrap_or(0.0);
                            br + vr
                        };
                        score_chunk(seq_a)
                            .partial_cmp(&score_chunk(seq_b))
                            .unwrap_or(Ordering::Equal)
                            .then_with(|| seq_b.cmp(&seq_a))
                    })
                    .unwrap_or(0)
            } else {
                0
            };

            scored.push(ScoredCandidate {
                path,
                blob_id: *blob_id,
                chunk_seq: best_chunk_seq,
                score: total_score,
                bm25_rank: bm25_entry.map(|(r, _)| r),
                vector_rank: vec_entry.map(|(r, _)| r),
                graph_score: g_score,
            });
        }

        scored.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(Ordering::Equal)
                .then_with(|| a.path.cmp(&b.path))
        });

        // 11. Fetch snippets for MMR diversity window + adjacent chunk merging, then pick top_k.
        let mmr_window = (req.top_k * 3).max(req.top_k).min(scored.len());
        let mut chunk_keys: Vec<(BlobId, usize)> = Vec::with_capacity(mmr_window * 2);
        for cand in scored.iter().take(mmr_window) {
            chunk_keys.push((cand.blob_id, cand.chunk_seq));
            let next_seq = cand.chunk_seq + 1;
            if bm25_chunk_rank.contains_key(&(cand.blob_id, next_seq))
                || vec_chunk_rank.contains_key(&(cand.blob_id, next_seq))
            {
                chunk_keys.push((cand.blob_id, next_seq));
            }
        }
        let chunk_details = self
            .vcs
            .store()
            .with_conn(|conn| fetch_chunk_details(conn, &chunk_keys))?;

        let mut pool: Vec<(ScoredCandidate, ChunkDetail, HashSet<String>)> =
            Vec::with_capacity(mmr_window);
        for cand in scored.into_iter().take(mmr_window) {
            let mut detail = chunk_details
                .get(&(cand.blob_id, cand.chunk_seq))
                .cloned()
                .unwrap_or_else(|| ChunkDetail {
                    title: title_by_blob
                        .get(&cand.blob_id)
                        .cloned()
                        .unwrap_or_else(|| cand.path.as_str().to_string()),
                    heading_path: "(root)".to_string(),
                    raw_snippet: String::new(),
                    start_line: 1,
                    end_line: 1,
                });

            let next_seq = cand.chunk_seq + 1;
            if bm25_chunk_rank.contains_key(&(cand.blob_id, next_seq))
                || vec_chunk_rank.contains_key(&(cand.blob_id, next_seq))
            {
                if let Some(next_detail) = chunk_details.get(&(cand.blob_id, next_seq)) {
                    if detail.heading_path == next_detail.heading_path
                        && detail.raw_snippet.len() + next_detail.raw_snippet.len()
                            <= self.config.chunk_max_chars * 2
                    {
                        detail.raw_snippet =
                            format!("{}\n{}", detail.raw_snippet, next_detail.raw_snippet);
                        detail.end_line = next_detail.end_line;
                    }
                }
            }
            let tokens = snippet_tokens(&detail.raw_snippet);
            pool.push((cand, detail, tokens));
        }

        let mut selected: Vec<(ScoredCandidate, ChunkDetail, HashSet<String>)> =
            Vec::with_capacity(req.top_k.min(pool.len()));
        while selected.len() < req.top_k && !pool.is_empty() {
            let mut best_idx = 0usize;
            let mut best_mmr = f32::NEG_INFINITY;
            for (idx, (cand, _, toks)) in pool.iter().enumerate() {
                let redundant = !toks.is_empty()
                    && selected.iter().any(|(sel_cand, _, sel_toks)| {
                        sel_cand.path.tier() != MemoryTier::Episodic
                            && token_jaccard(toks, sel_toks) >= 0.80
                    });
                let eff_score = if redundant {
                    cand.score * 0.55
                } else {
                    cand.score
                };
                if eff_score > best_mmr
                    || (eff_score == best_mmr && cand.path < pool[best_idx].0.path)
                {
                    best_mmr = eff_score;
                    best_idx = idx;
                }
            }
            selected.push(pool.remove(best_idx));
        }

        let mut results = Vec::with_capacity(selected.len());
        let mut hit_telemetry_pairs: Vec<(VirtualPath, BlobId)> =
            Vec::with_capacity(selected.len());
        for (cand, detail, _) in selected {
            let outlinks = outlinks_map
                .get(&cand.path)
                .map(|s| s.iter().cloned().collect())
                .unwrap_or_default();
            let backlinks = backlinks_map
                .get(&cand.path)
                .map(|s| s.iter().cloned().collect())
                .unwrap_or_default();

            hit_telemetry_pairs.push((cand.path.clone(), cand.blob_id));
            results.push(SearchHit {
                path: cand.path,
                blob_id: cand.blob_id,
                title: detail.title,
                heading_path: detail.heading_path,
                snippet: detail.raw_snippet,
                start_line: detail.start_line,
                end_line: detail.end_line,
                score: cand.score,
                bm25_rank: cand.bm25_rank,
                vector_rank: cand.vector_rank,
                graph_score: cand.graph_score,
                outlinks,
                backlinks,
            });
        }

        if let Some(branch) = branch_for_telemetry {
            let _ = record_search_hits(self.vcs.store(), branch, &hit_telemetry_pairs);
        }

        Ok(results)
    }

    /// Compiles `/core` memories for `rev` within `max_tokens` in `< 5ms` without LLM calls.
    pub fn compile_core(&self, rev: &Revision, max_tokens: usize) -> Result<CompiledCoreContext> {
        compile_core(&self.vcs, rev, max_tokens)
    }

    /// Imports a local Obsidian Vault directory into `branch` under `target_tier` and indexes it.
    pub fn import_vault(
        &self,
        vault_dir: &std::path::Path,
        branch: &str,
        target_tier: mem_core::MemoryTier,
    ) -> Result<VaultImportStats> {
        import_vault(self, vault_dir, branch, target_tier)
    }

    /// Syncs a multi-tier local Obsidian Vault (`core/`, `semantic/`, `procedural/`, `episodic/`) into `branch`.
    pub fn sync_vault(
        &self,
        vault_dir: &std::path::Path,
        branch: &str,
    ) -> Result<VaultImportStats> {
        sync_vault(self, vault_dir, branch)
    }

    /// Exports all active nodes in `rev` into a local Obsidian Vault directory.
    pub fn export_vault(
        &self,
        rev: &Revision,
        vault_dir: &std::path::Path,
    ) -> Result<VaultExportStats> {
        export_vault(&self.vcs, rev, vault_dir)
    }

    /// Structures a rough note into an auto-linked Obsidian Markdown card and commits/indexes it.
    pub fn ingest(
        &self,
        req: &IngestRequest,
        llm_config: Option<&ConsolidateLlmConfig>,
    ) -> Result<IngestOutcome> {
        ingest_knowledge(self, req, llm_config)
    }

    /// Computes a strictly branch-isolated Wiki link graph over active files in `rev`.
    pub fn branch_graph(&self, rev: &Revision) -> Result<BranchGraph> {
        let branch = match rev {
            Revision::BranchWorkspace(b) | Revision::BranchHead(b) => b.clone(),
            Revision::Commit(cid) => cid.to_hex(),
        };
        let (snap, ref_ms) = self.load_or_build_snapshot(rev)?;
        let active = snap.active_entries(None, ref_ms);
        self.ensure_snapshot_indexed(&snap, None, &active)?;

        let active_sig = snap.active_signature(ref_ms);
        if let Ok(guard) = snap.cached_branch_graph.lock() {
            if let Some((sig, ref cached)) = *guard {
                if sig == active_sig {
                    return Ok((**cached).clone());
                }
            }
        }

        let mut paths_by_blob: HashMap<BlobId, Vec<VirtualPath>> = HashMap::new();
        for (path, blob_id, _) in &active {
            paths_by_blob
                .entry(*blob_id)
                .or_default()
                .push(path.clone());
        }

        let (headers_by_blob, raw_links) = self.vcs.store().with_conn(|conn| {
            conn.execute_batch(
                "CREATE TEMP TABLE IF NOT EXISTS _mem_graph_active_blobs (blob_id TEXT PRIMARY KEY);
                 DELETE FROM _mem_graph_active_blobs;",
            )
            .map_err(|e| {
                MemError::Storage(format!("failed to prepare graph temp blobs table: {e}"))
            })?;

            {
                let mut insert_stmt = conn
                    .prepare_cached(
                        "INSERT OR IGNORE INTO _mem_graph_active_blobs (blob_id) VALUES (?1)",
                    )
                    .map_err(|e| {
                        MemError::Storage(format!("failed to prepare graph temp blob insert: {e}"))
                    })?;
                for blob_id in paths_by_blob.keys() {
                    insert_stmt
                        .execute(params![blob_id.to_hex()])
                        .map_err(|e| {
                            MemError::Storage(format!("failed to insert graph temp blob: {e}"))
                        })?;
                }
            }

            let res = (|| {
                let mut headers_map: HashMap<BlobId, (String, Vec<String>)> = HashMap::new();
                {
                    let mut stmt = conn
                        .prepare_cached(
                            "SELECT c.blob_id, c.title, c.aliases
                             FROM chunk_index AS c
                             INNER JOIN _mem_graph_active_blobs AS a ON a.blob_id = c.blob_id
                             WHERE c.chunk_seq = 0",
                        )
                        .map_err(|e| {
                            MemError::Storage(format!("failed to prepare graph header query: {e}"))
                        })?;
                    let rows = stmt
                        .query_map([], |r| {
                            Ok((
                                r.get::<_, String>(0)?,
                                r.get::<_, String>(1)?,
                                r.get::<_, String>(2)?,
                            ))
                        })
                        .map_err(|e| {
                            MemError::Storage(format!("failed to query graph headers: {e}"))
                        })?;
                    for row in rows {
                        let (hex, title, aliases_joined) = row.map_err(|e| {
                            MemError::Storage(format!("failed to decode graph header row: {e}"))
                        })?;
                        let blob_id = BlobId::from_hex(&hex)?;
                        let aliases = if aliases_joined.is_empty() {
                            Vec::new()
                        } else {
                            aliases_joined
                                .split(" | ")
                                .map(|s| s.trim().to_string())
                                .filter(|s| !s.is_empty())
                                .collect()
                        };
                        headers_map.insert(blob_id, (title, aliases));
                    }
                }

                let mut links: Vec<(BlobId, &'static str, String)> = Vec::new();
                {
                    let mut stmt = conn
                        .prepare_cached(
                            "SELECT l.source_blob_id, l.target_ref
                             FROM blob_links AS l
                             INNER JOIN _mem_graph_active_blobs AS a ON a.blob_id = l.source_blob_id
                             WHERE l.link_kind = 'wikilink'",
                        )
                        .map_err(|e| {
                            MemError::Storage(format!("failed to prepare graph links query: {e}"))
                        })?;
                    let rows = stmt
                        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
                        .map_err(|e| {
                            MemError::Storage(format!("failed to query graph wikilinks: {e}"))
                        })?;
                    for row in rows {
                        let (hex, target_ref) = row.map_err(|e| {
                            MemError::Storage(format!("failed to decode graph link row: {e}"))
                        })?;
                        links.push((BlobId::from_hex(&hex)?, "wikilink", target_ref));
                    }
                }

                // Also inspect active chunk snippets for standard `[label](target.md)` links so
                // vaults indexed before markdown-link extraction surface full graph edges immediately.
                {
                    let mut stmt = conn
                        .prepare_cached(
                            "SELECT c.blob_id, c.raw_snippet
                             FROM chunk_index AS c
                             INNER JOIN _mem_graph_active_blobs AS a ON a.blob_id = c.blob_id
                             ORDER BY c.blob_id, c.chunk_seq",
                        )
                        .map_err(|e| {
                            MemError::Storage(format!(
                                "failed to prepare graph markdown-link query: {e}"
                            ))
                        })?;
                    let rows = stmt
                        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
                        .map_err(|e| {
                            MemError::Storage(format!(
                                "failed to query chunk snippets for graph links: {e}"
                            ))
                        })?;
                    for row in rows {
                        let (hex, snippet) = row.map_err(|e| {
                            MemError::Storage(format!("failed to decode snippet row: {e}"))
                        })?;
                        let blob_id = BlobId::from_hex(&hex)?;
                        for mdlink in extract_markdown_links_from_markdown(&snippet) {
                            links.push((blob_id, "mdlink", mdlink.target));
                        }
                    }
                }

                Ok((headers_map, links))
            })();

            let _ = conn.execute_batch("DELETE FROM _mem_graph_active_blobs;");
            res
        })?;

        let mut wiki_resolver: HashMap<String, Vec<VirtualPath>> = HashMap::new();
        let mut active_paths: Vec<VirtualPath> = Vec::with_capacity(active.len());
        let mut active_paths_by_lower: HashMap<String, VirtualPath> =
            HashMap::with_capacity(active.len());

        for (path, blob_id, _) in &active {
            active_paths.push(path.clone());
            active_paths_by_lower.insert(path.as_str().to_lowercase(), path.clone());
            let (title, aliases) = headers_by_blob
                .get(blob_id)
                .map(|(t, a)| (t.as_str(), a.as_slice()))
                .unwrap_or_else(|| {
                    let fallback = path
                        .file_name()
                        .and_then(|n| n.strip_suffix(".md").or(Some(n)))
                        .unwrap_or_else(|| path.as_str());
                    (fallback, &[])
                });
            register_resolver_keys(&mut wiki_resolver, path, title, aliases);
        }

        let mut unique_edges: BTreeMap<(String, String), (&'static str, String)> = BTreeMap::new();
        let mut out_degrees: HashMap<String, usize> = HashMap::new();
        let mut in_degrees: HashMap<String, usize> = HashMap::new();

        for (source_blob, link_kind, target_ref) in raw_links {
            let Some(source_paths) = paths_by_blob.get(&source_blob) else {
                continue;
            };
            for src in source_paths {
                let Some(target_paths) = resolve_branch_graph_target(
                    src,
                    &target_ref,
                    &wiki_resolver,
                    &active_paths_by_lower,
                    &active_paths,
                ) else {
                    continue;
                };
                for tgt in &target_paths {
                    if src != tgt {
                        let src_path = src.as_str().to_string();
                        let tgt_path = tgt.as_str().to_string();
                        if let BTreeEntry::Vacant(entry) =
                            unique_edges.entry((src_path.clone(), tgt_path.clone()))
                        {
                            *out_degrees.entry(src_path).or_insert(0) += 1;
                            *in_degrees.entry(tgt_path).or_insert(0) += 1;
                            entry.insert((link_kind, target_ref.clone()));
                        }
                    }
                }
            }
        }

        let nodes = active
            .iter()
            .map(|(path, blob_id, meta)| {
                let p_str = path.as_str().to_string();
                let title = headers_by_blob
                    .get(blob_id)
                    .map(|(t, _)| t.clone())
                    .filter(|t| !t.trim().is_empty())
                    .unwrap_or_else(|| {
                        path.file_name()
                            .map(|f| f.strip_suffix(".md").unwrap_or(f).to_string())
                            .unwrap_or_else(|| p_str.clone())
                    });
                GraphNode {
                    id: p_str.clone(),
                    path: p_str.clone(),
                    title,
                    tier: path.tier().as_str().to_string(),
                    importance: meta.importance,
                    tags: meta.tags.clone(),
                    blob_id: blob_id.to_hex(),
                    in_degree: in_degrees.get(&p_str).copied().unwrap_or(0),
                    out_degree: out_degrees.get(&p_str).copied().unwrap_or(0),
                }
            })
            .collect();

        let edges = unique_edges
            .into_iter()
            .map(|((source, target), (kind, label))| GraphEdge {
                source,
                target,
                kind: kind.to_string(),
                label,
            })
            .collect();

        let graph = BranchGraph {
            branch,
            nodes,
            edges,
        };
        if let Ok(mut guard) = snap.cached_branch_graph.lock() {
            *guard = Some((active_sig, Arc::new(graph.clone())));
        }
        Ok(graph)
    }
}

fn resolve_branch_graph_target(
    src: &VirtualPath,
    target_ref: &str,
    wiki_resolver: &HashMap<String, Vec<VirtualPath>>,
    active_paths_by_lower: &HashMap<String, VirtualPath>,
    active_paths: &[VirtualPath],
) -> Option<Vec<VirtualPath>> {
    let trimmed = target_ref.trim();
    if trimmed.is_empty() {
        return None;
    }

    if let Some(rel_resolved) = normalize_relative_virtual_path(src, trimmed) {
        if let Some(found) = active_paths_by_lower.get(&rel_resolved) {
            return Some(vec![found.clone()]);
        }
        if !rel_resolved.ends_with(".md") {
            let with_ext = format!("{rel_resolved}.md");
            if let Some(found) = active_paths_by_lower.get(&with_ext) {
                return Some(vec![found.clone()]);
            }
        }
    }

    let cleaned = trimmed
        .trim_start_matches("./")
        .trim_start_matches("../")
        .trim_start_matches('/')
        .to_lowercase();
    if cleaned.contains('/') {
        let suffix = format!("/{cleaned}");
        let suffix_md = if cleaned.ends_with(".md") {
            None
        } else {
            Some(format!("/{cleaned}.md"))
        };
        let suffix_matches: Vec<VirtualPath> = active_paths
            .iter()
            .filter(|p| {
                let low = p.as_str().to_lowercase();
                low.ends_with(&suffix) || suffix_md.as_deref().is_some_and(|sm| low.ends_with(sm))
            })
            .cloned()
            .collect();
        if suffix_matches.len() == 1 {
            return Some(suffix_matches);
        }
        if suffix_matches.len() > 1 {
            let src_tier = src.tier().as_str();
            let same_tier: Vec<VirtualPath> = suffix_matches
                .iter()
                .filter(|p| p.tier().as_str() == src_tier)
                .cloned()
                .collect();
            if same_tier.len() == 1 {
                return Some(same_tier);
            }
            return Some(suffix_matches);
        }
    }

    resolve_wiki_target(wiki_resolver, trimmed)
}

fn normalize_relative_virtual_path(src: &VirtualPath, target_ref: &str) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    if target_ref.starts_with('/') {
        for seg in target_ref.split('/') {
            match seg {
                "" | "." => {}
                ".." => {
                    parts.pop();
                }
                s => parts.push(s.to_lowercase()),
            }
        }
    } else {
        if let Some(parent) = src.as_str().rsplit_once('/').map(|(dir, _)| dir) {
            for seg in parent.split('/') {
                if !seg.is_empty() {
                    parts.push(seg.to_lowercase());
                }
            }
        }
        for seg in target_ref.split('/') {
            match seg {
                "" | "." => {}
                ".." => {
                    parts.pop();
                }
                s => parts.push(s.to_lowercase()),
            }
        }
    }
    if parts.is_empty() {
        None
    } else {
        Some(format!("/{}", parts.join("/")))
    }
}

#[derive(Clone)]
struct ActiveBlobHeader {
    title: String,
    aliases: Vec<String>,
}

#[derive(Clone)]
struct FtsHit {
    blob_id: BlobId,
    chunk_seq: usize,
    score: f64,
}

#[derive(Clone)]
struct ActiveLinkRow {
    source_blob_id: BlobId,
    link_kind: String,
    source_entity: String,
    target_ref: String,
}

struct ActiveSearchData {
    headers: HashMap<BlobId, ActiveBlobHeader>,
    fts_hits: Vec<FtsHit>,
    lexical_fallback: Vec<(BlobId, usize)>,
    vec_hits: Vec<(BlobId, usize, f32)>,
    vec_chunk_hits: Vec<(BlobId, usize, f32)>,
    loaded_vecs: Option<Vec<CachedChunkVector>>,
    links: Vec<ActiveLinkRow>,
    telemetry: HashMap<VirtualPath, TelemetryRow>,
}

#[derive(Clone)]
struct ChunkDetail {
    title: String,
    heading_path: String,
    raw_snippet: String,
    start_line: usize,
    end_line: usize,
}

fn current_model_index_count(conn: &Connection, model: &str) -> Result<u64> {
    let count: i64 = if model == DISABLED_MODEL {
        conn.query_row("SELECT COUNT(DISTINCT blob_id) FROM chunk_index", [], |r| {
            r.get(0)
        })
        .map_err(|e| MemError::Storage(format!("failed to count chunk_index blobs: {e}")))?
    } else {
        conn.query_row(
            "SELECT COUNT(*) FROM blob_index_model WHERE model = ?1",
            params![model],
            |r| r.get(0),
        )
        .map_err(|e| MemError::Storage(format!("failed to count blob_index_model rows: {e}")))?
    };
    Ok(u64::try_from(count.max(0)).unwrap_or(0))
}

fn build_resolved_graph(
    paths_by_blob: &HashMap<BlobId, Vec<VirtualPath>>,
    headers: &HashMap<BlobId, ActiveBlobHeader>,
    links: &[ActiveLinkRow],
) -> CachedResolvedGraph {
    let mut wiki_resolver: HashMap<String, Vec<VirtualPath>> = HashMap::new();
    let mut active_paths: Vec<VirtualPath> = Vec::new();
    let mut active_paths_by_lower: HashMap<String, VirtualPath> = HashMap::new();

    for (blob_id, paths) in paths_by_blob {
        let header = headers.get(blob_id);
        for path in paths {
            active_paths.push(path.clone());
            active_paths_by_lower.insert(path.as_str().to_lowercase(), path.clone());
            if let Some(h) = header {
                register_resolver_keys(&mut wiki_resolver, path, &h.title, &h.aliases);
            } else {
                let fallback = path
                    .file_name()
                    .and_then(|n| n.strip_suffix(".md").or(Some(n)))
                    .unwrap_or_else(|| path.as_str());
                register_resolver_keys(&mut wiki_resolver, path, fallback, &[]);
            }
        }
    }

    let mut outlinks_map: HashMap<VirtualPath, BTreeSet<VirtualPath>> = HashMap::new();
    let mut backlinks_map: HashMap<VirtualPath, BTreeSet<VirtualPath>> = HashMap::new();
    let mut entity_nodes: HashMap<String, BTreeSet<VirtualPath>> = HashMap::new();

    for link in links {
        let Some(source_paths) = paths_by_blob.get(&link.source_blob_id) else {
            continue;
        };

        match link.link_kind.as_str() {
            "wikilink" => {
                for src in source_paths {
                    if let Some(targets) = resolve_branch_graph_target(
                        src,
                        &link.target_ref,
                        &wiki_resolver,
                        &active_paths_by_lower,
                        &active_paths,
                    ) {
                        for tgt in &targets {
                            if src != tgt {
                                outlinks_map
                                    .entry(src.clone())
                                    .or_default()
                                    .insert(tgt.clone());
                                backlinks_map
                                    .entry(tgt.clone())
                                    .or_default()
                                    .insert(src.clone());
                            }
                        }
                    }
                }
            }
            "entity_rel" => {
                let key_target = normalize_lookup_key(&link.target_ref);
                let key_source = normalize_lookup_key(&link.source_entity);
                for src in source_paths {
                    if !key_target.is_empty() {
                        entity_nodes
                            .entry(key_target.clone())
                            .or_default()
                            .insert(src.clone());
                    }
                    if !key_source.is_empty() {
                        entity_nodes
                            .entry(key_source.clone())
                            .or_default()
                            .insert(src.clone());
                    }
                }
            }
            _ => {}
        }
    }

    let out_degrees: HashMap<VirtualPath, usize> = outlinks_map
        .iter()
        .map(|(k, v)| (k.clone(), v.len()))
        .collect();
    let in_degrees: HashMap<VirtualPath, usize> = backlinks_map
        .iter()
        .map(|(k, v)| (k.clone(), v.len()))
        .collect();

    CachedResolvedGraph {
        outlinks_map,
        backlinks_map,
        entity_nodes,
        out_degrees,
        in_degrees,
    }
}

type VectorScoreResult = (Vec<(BlobId, usize, f32)>, Vec<(BlobId, usize, f32)>);

fn score_chunk_vectors(
    q_raw: &[f32],
    chunks: &[CachedChunkVector],
    blob_filter: Option<&HashMap<BlobId, Vec<VirtualPath>>>,
    fts_limit: usize,
) -> Result<VectorScoreResult> {
    if chunks.is_empty() || q_raw.is_empty() {
        return Ok((Vec::new(), Vec::new()));
    }
    let q_unit = normalize_vec(q_raw);
    let q_bq = decode_bq_words(&encode_bq_vec(&q_unit));
    let dim = q_unit.len();

    let use_bq_coarse = dim >= 64
        && chunks.len() > 256
        && !q_bq.is_empty()
        && chunks.iter().all(|c| !c.bq_words.is_empty());

    let candidate_indices: Vec<usize> = if use_bq_coarse {
        let mut rough: Vec<(usize, u32)> = chunks
            .iter()
            .enumerate()
            .filter(|(_, c)| blob_filter.is_none_or(|f| f.contains_key(&c.blob_id)))
            .map(|(idx, c)| (idx, bq_hamming_distance_words(&q_bq, &c.bq_words)))
            .collect();
        let coarse_k = (fts_limit * 4).max(256);
        if rough.len() > coarse_k {
            rough.select_nth_unstable_by_key(coarse_k - 1, |&(_, dist)| dist);
            rough.truncate(coarse_k);
        }
        rough.into_iter().map(|(idx, _)| idx).collect()
    } else {
        (0..chunks.len())
            .filter(|&idx| blob_filter.is_none_or(|f| f.contains_key(&chunks[idx].blob_id)))
            .collect()
    };

    let mut vec_chunk_hits = Vec::new();
    let mut vec_best_by_blob: HashMap<BlobId, (usize, f32)> = HashMap::new();
    for idx in candidate_indices {
        let c = &chunks[idx];
        let sim = if !c.bq_words.is_empty() {
            dot_product_le_bytes(&q_unit, &c.f32_bytes)?
        } else {
            cosine_similarity_le_bytes(&q_unit, &c.f32_bytes)?
        };
        if sim <= 0.0 {
            continue;
        }
        vec_chunk_hits.push((c.blob_id, c.chunk_seq, sim));
        vec_best_by_blob
            .entry(c.blob_id)
            .and_modify(|cur| {
                if sim > cur.1 {
                    *cur = (c.chunk_seq, sim);
                }
            })
            .or_insert((c.chunk_seq, sim));
    }

    vec_chunk_hits.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap_or(Ordering::Equal));
    vec_chunk_hits.truncate(fts_limit * 2);

    let mut vec_hits: Vec<(BlobId, usize, f32)> = vec_best_by_blob
        .into_iter()
        .map(|(b, (seq, s))| (b, seq, s))
        .collect();
    vec_hits.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap_or(Ordering::Equal));
    vec_hits.truncate(fts_limit);

    Ok((vec_hits, vec_chunk_hits))
}

fn load_active_search_data(
    conn: &Connection,
    paths_by_blob: &HashMap<BlobId, Vec<VirtualPath>>,
    query: &str,
    load_vecs_model: Option<&str>,
    fts_limit: usize,
    need_graph_load: bool,
    telemetry_branch: Option<&str>,
) -> Result<ActiveSearchData> {
    conn.execute_batch(
        "CREATE TEMP TABLE IF NOT EXISTS _mem_active_blobs (blob_id TEXT PRIMARY KEY);
         DELETE FROM _mem_active_blobs;",
    )
    .map_err(|e| MemError::Storage(format!("failed to prepare temp active blobs table: {e}")))?;

    {
        let mut insert_stmt = conn
            .prepare_cached("INSERT OR IGNORE INTO _mem_active_blobs (blob_id) VALUES (?1)")
            .map_err(|e| MemError::Storage(format!("failed to prepare temp blob insert: {e}")))?;
        for blob_id in paths_by_blob.keys() {
            insert_stmt
                .execute(params![blob_id.to_hex()])
                .map_err(|e| MemError::Storage(format!("failed to insert temp blob: {e}")))?;
        }
    }

    let result = (|| {
        // 1. Load headers (title + aliases from chunk_seq = 0) when not cached.
        let mut headers = HashMap::new();
        if need_graph_load {
            let mut stmt = conn
                .prepare_cached(
                    "SELECT c.blob_id, c.title, c.aliases
                     FROM chunk_index AS c
                     INNER JOIN _mem_active_blobs AS a ON a.blob_id = c.blob_id
                     WHERE c.chunk_seq = 0",
                )
                .map_err(|e| MemError::Storage(format!("failed to prepare header query: {e}")))?;
            let rows = stmt
                .query_map([], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                    ))
                })
                .map_err(|e| MemError::Storage(format!("failed to query active headers: {e}")))?;
            for row in rows {
                let (hex, title, aliases_joined) = row
                    .map_err(|e| MemError::Storage(format!("failed to decode header row: {e}")))?;
                let aliases = if aliases_joined.is_empty() {
                    Vec::new()
                } else {
                    aliases_joined
                        .split(" | ")
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty())
                        .collect()
                };
                headers.insert(BlobId::from_hex(&hex)?, ActiveBlobHeader { title, aliases });
            }
        }

        // 2. Run two-stage FTS5 multi-column weighted BM25 query filtered strictly by _mem_active_blobs.
        let mut fts_hits = Vec::new();
        let mut seen_fts_chunks: HashSet<(BlobId, usize)> = HashSet::new();
        let fts_plan = build_fts5_queries(query);

        if let Some(ref and_q) = fts_plan.strict_and {
            execute_fts_query(
                conn,
                and_q,
                fts_limit,
                -1000.0,
                &mut seen_fts_chunks,
                &mut fts_hits,
            )?;
        }
        if fts_hits.len() < fts_limit {
            if let Some(ref or_q) = fts_plan.relaxed_or {
                execute_fts_query(
                    conn,
                    or_q,
                    fts_limit,
                    0.0,
                    &mut seen_fts_chunks,
                    &mut fts_hits,
                )?;
            }
        }

        // 3. Fast SQL-filtered lexical fallback only when FTS5 produced no hits (e.g. single CJK char or partial symbol).
        let mut lexical_fallback = Vec::new();
        if fts_hits.is_empty() && !query.trim().is_empty() {
            let mut stmt = conn
                .prepare_cached(
                    "SELECT c.blob_id, c.chunk_seq, c.title, c.aliases, c.tags, c.heading_path, c.raw_snippet
                     FROM chunk_index AS c
                     INNER JOIN _mem_active_blobs AS a ON a.blob_id = c.blob_id
                     WHERE INSTR(LOWER(c.title), LOWER(?1)) > 0
                        OR INSTR(LOWER(c.aliases), LOWER(?1)) > 0
                        OR INSTR(LOWER(c.tags), LOWER(?1)) > 0
                        OR INSTR(LOWER(c.raw_snippet), LOWER(?1)) > 0",
                )
                .map_err(|e| {
                    MemError::Storage(format!("failed to prepare lexical fallback query: {e}"))
                })?;
            let rows = stmt
                .query_map(params![query.trim()], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, i64>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, String>(3)?,
                        r.get::<_, String>(4)?,
                        r.get::<_, String>(5)?,
                        r.get::<_, String>(6)?,
                    ))
                })
                .map_err(|e| {
                    MemError::Storage(format!(
                        "failed to query active chunks for lexical scan: {e}"
                    ))
                })?;

            let mut scored_lex: Vec<(BlobId, usize, f32)> = Vec::new();
            for row in rows {
                let (hex, seq, title, aliases, tags, heading, body) = row.map_err(|e| {
                    MemError::Storage(format!("failed to decode lexical fallback row: {e}"))
                })?;
                let lex_score =
                    score_lexical_match(query, &title, &aliases, &tags, &heading, &body);
                if lex_score > 0.0 {
                    scored_lex.push((
                        BlobId::from_hex(&hex)?,
                        usize::try_from(seq.max(0)).unwrap_or(0),
                        lex_score,
                    ));
                }
            }
            scored_lex.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap_or(Ordering::Equal));
            for (b, seq, _) in scored_lex.into_iter().take(fts_limit) {
                lexical_fallback.push((b, seq));
            }
        }

        // 4. Load active embeddings for the active model when not cached in snapshot.
        let mut loaded_vecs = None;
        if let Some(model) = load_vecs_model {
            let mut stmt = conn
                .prepare_cached(
                    // Only vectors of the active model: another model's vectors (not yet
                    // re-embedded) give meaningless similarities.
                    "SELECT c.blob_id, c.chunk_seq, c.embedding, c.embedding_bq
                     FROM chunk_index AS c
                     INNER JOIN _mem_active_blobs AS a ON a.blob_id = c.blob_id
                     INNER JOIN blob_index_model AS m ON m.blob_id = c.blob_id AND m.model = ?1
                     WHERE c.embedding IS NOT NULL",
                )
                .map_err(|e| {
                    MemError::Storage(format!("failed to prepare embedding query: {e}"))
                })?;
            let mut rows = stmt.query(params![model]).map_err(|e| {
                MemError::Storage(format!("failed to query active embeddings: {e}"))
            })?;
            let mut vecs = Vec::new();
            while let Some(row) = rows
                .next()
                .map_err(|e| MemError::Storage(format!("failed to step embedding row: {e}")))?
            {
                let hex = row
                    .get_ref(0)
                    .map_err(|e| MemError::Storage(format!("failed to read blob_id ref: {e}")))?
                    .as_str()
                    .map_err(|e| MemError::Storage(format!("invalid blob_id ref type: {e}")))?;
                let seq_i64 = row
                    .get::<_, i64>(1)
                    .map_err(|e| MemError::Storage(format!("failed to read chunk_seq: {e}")))?;
                let f32_bytes = row
                    .get_ref(2)
                    .map_err(|e| MemError::Storage(format!("failed to read embedding ref: {e}")))?
                    .as_bytes()
                    .map_err(|e| MemError::Storage(format!("invalid embedding blob type: {e}")))?
                    .to_vec();
                let bq_words = match row.get_ref(3) {
                    Ok(rusqlite::types::ValueRef::Blob(b)) => decode_bq_words(b),
                    _ => Vec::new(),
                };
                vecs.push(CachedChunkVector {
                    blob_id: BlobId::from_hex(hex)?,
                    chunk_seq: usize::try_from(seq_i64.max(0)).unwrap_or(0),
                    f32_bytes,
                    bq_words,
                });
            }
            loaded_vecs = Some(vecs);
        }

        // 5. Load branch-scoped graph edges from blob_links when not cached.
        let mut links = Vec::new();
        if need_graph_load {
            let mut stmt = conn
                .prepare_cached(
                    "SELECT l.source_blob_id, l.link_kind, l.source_entity, l.target_ref
                     FROM blob_links AS l
                     INNER JOIN _mem_active_blobs AS a ON a.blob_id = l.source_blob_id",
                )
                .map_err(|e| MemError::Storage(format!("failed to prepare graph query: {e}")))?;
            let rows = stmt
                .query_map([], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, String>(3)?,
                    ))
                })
                .map_err(|e| {
                    MemError::Storage(format!("failed to query active blob_links: {e}"))
                })?;
            for row in rows {
                let (hex, kind, source_entity, target_ref) =
                    row.map_err(|e| MemError::Storage(format!("failed to decode link row: {e}")))?;
                links.push(ActiveLinkRow {
                    source_blob_id: BlobId::from_hex(&hex)?,
                    link_kind: kind,
                    source_entity,
                    target_ref,
                });
            }
        }

        let telemetry = match telemetry_branch {
            Some(branch) => load_branch_telemetry(conn, branch)?,
            None => HashMap::new(),
        };

        Ok(ActiveSearchData {
            headers,
            fts_hits,
            lexical_fallback,
            vec_hits: Vec::new(),
            vec_chunk_hits: Vec::new(),
            loaded_vecs,
            links,
            telemetry,
        })
    })();

    let _ = conn.execute("DELETE FROM _mem_active_blobs", []);
    result
}

fn execute_fts_query(
    conn: &Connection,
    fts_query: &str,
    fts_limit: usize,
    score_offset: f64,
    seen_fts_chunks: &mut HashSet<(BlobId, usize)>,
    fts_hits: &mut Vec<FtsHit>,
) -> Result<()> {
    let mut stmt = conn
        .prepare_cached(
            "SELECT f.blob_id, f.chunk_seq,
                    bm25(fts_chunks, 0.0, 0.0, 5.0, 5.0, 3.0, 2.0, 1.0) AS rank_score
             FROM fts_chunks AS f
             INNER JOIN _mem_active_blobs AS a ON a.blob_id = f.blob_id
             WHERE fts_chunks MATCH ?1
             ORDER BY rank_score ASC
             LIMIT ?2",
        )
        .map_err(|e| MemError::Storage(format!("failed to prepare FTS5 query: {e}")))?;
    let rows = stmt
        .query_map(
            params![fts_query, i64::try_from(fts_limit).unwrap_or(100)],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, f64>(2)?,
                ))
            },
        )
        .map_err(|e| MemError::Storage(format!("failed to execute FTS5 query: {e}")))?;
    for row in rows {
        let (hex, seq, score) =
            row.map_err(|e| MemError::Storage(format!("failed to decode FTS5 row: {e}")))?;
        let blob_id = BlobId::from_hex(&hex)?;
        let chunk_seq = usize::try_from(seq.max(0)).unwrap_or(0);
        if seen_fts_chunks.insert((blob_id, chunk_seq)) {
            fts_hits.push(FtsHit {
                blob_id,
                chunk_seq,
                score: score + score_offset,
            });
        }
    }
    Ok(())
}

fn fetch_chunk_details(
    conn: &Connection,
    keys: &[(BlobId, usize)],
) -> Result<HashMap<(BlobId, usize), ChunkDetail>> {
    let mut out = HashMap::new();
    let mut stmt = conn
        .prepare_cached(
            "SELECT title, heading_path, raw_snippet, start_line, end_line
             FROM chunk_index WHERE blob_id = ?1 AND chunk_seq = ?2",
        )
        .map_err(|e| MemError::Storage(format!("failed to prepare chunk detail query: {e}")))?;

    for &(blob_id, seq) in keys {
        if out.contains_key(&(blob_id, seq)) {
            continue;
        }
        let seq_i64 = i64::try_from(seq).unwrap_or(0);
        let row = stmt.query_row(params![blob_id.to_hex(), seq_i64], |r| {
            Ok(ChunkDetail {
                title: r.get(0)?,
                heading_path: r.get(1)?,
                raw_snippet: r.get(2)?,
                start_line: usize::try_from(r.get::<_, i64>(3)?.max(1)).unwrap_or(1),
                end_line: usize::try_from(r.get::<_, i64>(4)?.max(1)).unwrap_or(1),
            })
        });
        if let Ok(detail) = row {
            out.insert((blob_id, seq), detail);
        }
    }
    Ok(out)
}

fn is_cjk(c: char) -> bool {
    matches!(
        c as u32,
        0x4E00..=0x9FFF | 0x3400..=0x4DBF | 0x3040..=0x30FF | 0xAC00..=0xD7AF
    )
}

/// Appends space-separated CJK bigrams so FTS5 `unicode61` indexes Chinese/Japanese/Korean phrases natively.
fn augment_fts_text(text: &str) -> String {
    let mut bigrams = Vec::new();
    let mut run: Vec<char> = Vec::new();
    let flush_run = |run: &mut Vec<char>, out: &mut Vec<String>| {
        if run.len() >= 2 {
            for pair in run.windows(2) {
                let mut bg = String::with_capacity(8);
                bg.push(pair[0]);
                bg.push(pair[1]);
                out.push(bg);
            }
        }
        run.clear();
    };

    for ch in text.chars() {
        if is_cjk(ch) {
            run.push(ch);
        } else {
            flush_run(&mut run, &mut bigrams);
        }
    }
    flush_run(&mut run, &mut bigrams);

    if bigrams.is_empty() {
        text.to_string()
    } else {
        format!("{text} {}", bigrams.join(" "))
    }
}

struct FtsQueryPlan {
    strict_and: Option<String>,
    relaxed_or: Option<String>,
}

fn is_query_stopword(lower: &str) -> bool {
    matches!(
        lower,
        "如何"
            | "怎么"
            | "怎样"
            | "什么"
            | "哪些"
            | "为什么"
            | "请问"
            | "进行"
            | "使用"
            | "问题"
            | "的"
            | "了"
            | "吗"
            | "呢"
            | "是"
            | "在"
            | "和"
            | "与"
            | "及"
            | "或"
            | "the"
            | "a"
            | "an"
            | "how"
            | "what"
            | "why"
            | "when"
            | "where"
            | "which"
            | "who"
            | "is"
            | "are"
            | "was"
            | "were"
            | "to"
            | "of"
            | "in"
            | "on"
            | "for"
            | "with"
            | "by"
            | "at"
            | "from"
            | "as"
            | "into"
            | "about"
    )
}

fn build_fts5_queries(query: &str) -> FtsQueryPlan {
    let mut raw_segments: Vec<(String, Vec<String>)> = Vec::new();

    for raw in query.split(|c: char| !c.is_alphanumeric() && c != '_') {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            continue;
        }
        let lower = trimmed.to_lowercase();
        let mut seg_terms: Vec<String> = Vec::new();
        let mut seg_seen: HashSet<String> = HashSet::new();

        // FTS5 `unicode61` splits `_` into adjacent tokens, so quoting `"BUSY_TIMEOUT"`
        // forms an exact phrase `"busy timeout"` that matches cleanly.
        if seg_seen.insert(lower.clone()) {
            let escaped = trimmed.replace('"', "\"\"");
            seg_terms.push(format!("\"{escaped}\""));
        }

        let mut cjk_run: Vec<char> = Vec::new();
        let mut push_cjk_bigrams = |run: &mut Vec<char>| {
            if run.len() >= 2 {
                for pair in run.windows(2) {
                    let bg: String = pair.iter().collect();
                    if !is_query_stopword(&bg) && seg_seen.insert(bg.clone()) {
                        seg_terms.push(format!("\"{bg}\""));
                    }
                }
            }
            run.clear();
        };
        for ch in trimmed.chars() {
            if is_cjk(ch) {
                cjk_run.push(ch);
            } else {
                push_cjk_bigrams(&mut cjk_run);
            }
        }
        push_cjk_bigrams(&mut cjk_run);
        raw_segments.push((lower, seg_terms));
    }

    if raw_segments.is_empty() {
        return FtsQueryPlan {
            strict_and: None,
            relaxed_or: None,
        };
    }

    let non_stop: Vec<&(String, Vec<String>)> = raw_segments
        .iter()
        .filter(|(lower, _)| !is_query_stopword(lower))
        .collect();
    let chosen: Vec<&(String, Vec<String>)> = if non_stop.is_empty() {
        raw_segments.iter().collect()
    } else {
        non_stop
    };

    let mut flat_or_terms: Vec<String> = Vec::new();
    let mut seen_or: HashSet<String> = HashSet::new();
    let mut and_groups: Vec<String> = Vec::new();
    let mut seen_seg: HashSet<String> = HashSet::new();

    for (lower, seg_terms) in chosen {
        let mut group_terms = Vec::new();
        for t in seg_terms.iter().take(6) {
            group_terms.push(t.clone());
            if seen_or.insert(t.clone()) && flat_or_terms.len() < 16 {
                flat_or_terms.push(t.clone());
            }
        }
        if seen_seg.insert(lower.clone()) && !group_terms.is_empty() {
            if group_terms.len() == 1 {
                and_groups.push(group_terms.into_iter().next().unwrap_or_default());
            } else {
                and_groups.push(format!("({})", group_terms.join(" OR ")));
            }
        }
    }

    let strict_and = if and_groups.len() >= 2 {
        Some(and_groups.join(" AND "))
    } else {
        None
    };
    let relaxed_or = if flat_or_terms.is_empty() {
        None
    } else {
        Some(flat_or_terms.join(" OR "))
    };

    FtsQueryPlan {
        strict_and,
        relaxed_or,
    }
}

fn snippet_tokens(text: &str) -> HashSet<String> {
    let lower = text.to_lowercase();
    let mut toks = HashSet::new();
    for tok in lower.split(|c: char| !c.is_alphanumeric() && c != '_') {
        if tok.chars().count() >= 2 {
            toks.insert(tok.to_string());
        }
    }
    let cjk: Vec<char> = lower.chars().filter(|&c| is_cjk(c)).collect();
    for pair in cjk.windows(2) {
        let mut bg = String::with_capacity(8);
        bg.push(pair[0]);
        bg.push(pair[1]);
        toks.insert(bg);
    }
    toks
}

fn token_jaccard(a: &HashSet<String>, b: &HashSet<String>) -> f32 {
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let inter = a.intersection(b).count() as f32;
    let union = a.union(b).count() as f32;
    if union == 0.0 {
        0.0
    } else {
        inter / union
    }
}

fn score_lexical_match(
    query: &str,
    title: &str,
    aliases: &str,
    tags: &str,
    heading: &str,
    body: &str,
) -> f32 {
    let q_lower = query.trim().to_lowercase();
    if q_lower.is_empty() {
        return 0.0;
    }

    let t_lower = title.to_lowercase();
    let a_lower = aliases.to_lowercase();
    let g_lower = tags.to_lowercase();
    let h_lower = heading.to_lowercase();
    let b_lower = body.to_lowercase();

    let mut score = 0.0f32;
    if t_lower.contains(&q_lower) {
        score += 10.0;
    }
    if a_lower.contains(&q_lower) {
        score += 10.0;
    }
    if g_lower.contains(&q_lower) {
        score += 6.0;
    }
    if h_lower.contains(&q_lower) {
        score += 4.0;
    }
    if b_lower.contains(&q_lower) {
        score += 2.0;
    }

    // Score CJK character bigrams so multi-character Chinese queries match flexibly.
    let cjk_chars: Vec<char> = q_lower
        .chars()
        .filter(|c| {
            matches!(
                *c as u32,
                0x4E00..=0x9FFF | 0x3400..=0x4DBF | 0x3040..=0x30FF | 0xAC00..=0xD7AF
            )
        })
        .collect();
    if cjk_chars.len() >= 2 {
        let mut bigram = String::with_capacity(8);
        for pair in cjk_chars.windows(2) {
            bigram.clear();
            bigram.push(pair[0]);
            bigram.push(pair[1]);
            if t_lower.contains(&bigram) || a_lower.contains(&bigram) {
                score += 2.5;
            } else if g_lower.contains(&bigram) || h_lower.contains(&bigram) {
                score += 1.5;
            } else if b_lower.contains(&bigram) {
                score += 0.8;
            }
        }
    }

    score
}

fn normalize_lookup_key(raw: &str) -> String {
    raw.trim()
        .trim_start_matches("./")
        .trim_start_matches('/')
        .to_lowercase()
}

fn register_resolver_keys(
    resolver: &mut HashMap<String, Vec<VirtualPath>>,
    path: &VirtualPath,
    title: &str,
    aliases: &[String],
) {
    let mut keys = Vec::new();
    let full_lower = path.as_str().to_lowercase();
    keys.push(full_lower.clone());
    if let Some(no_ext) = full_lower.strip_suffix(".md") {
        keys.push(no_ext.to_string());
    }
    let no_leading = full_lower.trim_start_matches('/').to_string();
    keys.push(no_leading.clone());
    if let Some(no_ext_rel) = no_leading.strip_suffix(".md") {
        keys.push(no_ext_rel.to_string());
    }

    if let Some(file_name) = path.file_name() {
        let fname = file_name.to_lowercase();
        keys.push(fname.clone());
        if let Some(stem) = fname.strip_suffix(".md") {
            keys.push(stem.to_string());
        }
    }

    let t = title.trim().to_lowercase();
    if !t.is_empty() {
        keys.push(t);
    }

    for alias in aliases {
        let a = alias.trim().to_lowercase();
        if !a.is_empty() {
            keys.push(a);
        }
    }

    for key in keys {
        let entry = resolver.entry(key).or_default();
        if !entry.contains(path) {
            entry.push(path.clone());
        }
    }
}

fn resolve_wiki_target(
    resolver: &HashMap<String, Vec<VirtualPath>>,
    target_ref: &str,
) -> Option<Vec<VirtualPath>> {
    let exact_lower = target_ref.trim().to_lowercase();
    if let Some(found) = resolver.get(&exact_lower) {
        return Some(found.clone());
    }
    let no_ext = exact_lower.strip_suffix(".md").unwrap_or(&exact_lower);
    if let Some(found) = resolver.get(no_ext) {
        return Some(found.clone());
    }
    let normalized = normalize_lookup_key(target_ref);
    if let Some(found) = resolver.get(&normalized) {
        return Some(found.clone());
    }
    None
}

/// An indexed blob is fresh when no model is active (keep whatever exists, so an embedder
/// outage never drops vectors) or when its vectors were made by `model`.
fn index_is_fresh(conn: &Connection, blob_hex: &str, model: &str) -> Result<bool> {
    if model == DISABLED_MODEL {
        return Ok(true);
    }
    // No row (an index older than blob_index_model) means the model is unknown: stale.
    let stored: Option<String> = conn
        .prepare_cached("SELECT model FROM blob_index_model WHERE blob_id = ?1")
        .and_then(|mut stmt| stmt.query_row(params![blob_hex], |r| r.get(0)).optional())
        .map_err(|e| MemError::Storage(format!("failed to read index model: {e}")))?;
    Ok(stored.as_deref() == Some(model))
}
