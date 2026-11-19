//! Branch-aware hybrid search combining FTS5 BM25, vectors, and Obsidian 1-hop Wiki graph.

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
use std::sync::Arc;

use mem_core::{BlobId, MemError, MemoryMetadata, Result, Revision, VirtualPath};
use mem_store::cas::get_blob_conn;
use mem_vcs::MemVcs;
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};

use crate::compiler::{compile_core, CompiledCoreContext};
use crate::config::{ConsolidateLlmConfig, SearchConfig};
use crate::embedder::{
    cosine_similarity_le_bytes, encode_f32_vec, EmbeddingProvider, NoopEmbedder,
};
use crate::ingest::{ingest_knowledge, IngestOutcome, IngestRequest};
use crate::vault::{
    export_vault, import_vault, sync_vault, VaultExportStats, VaultImportStats,
};
use crate::wiki::{parse_wiki_document, ParsedWikiDocument};

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
    pub fn new(
        vcs: MemVcs,
        embedder: Arc<dyn EmbeddingProvider>,
        config: SearchConfig,
    ) -> Self {
        Self {
            vcs,
            embedder,
            config,
            total_embedding_calls: Arc::new(AtomicUsize::new(0)),
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
    pub fn index_unique_blobs(&self, items: &[(VirtualPath, BlobId)]) -> Result<IndexingStats> {
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

        // Step 1: Under short read lock, identify blobs missing from chunk_index and read raw bytes.
        struct PendingBlob {
            blob_id: BlobId,
            parsed: ParsedWikiDocument,
            embeddings: Option<Vec<Vec<f32>>>,
        }

        let mut missing_raw: Vec<(BlobId, VirtualPath, Vec<u8>)> = Vec::new();
        self.vcs.store().with_conn(|conn| {
            let mut exists_stmt = conn
                .prepare_cached("SELECT 1 FROM chunk_index WHERE blob_id = ?1 LIMIT 1")
                .map_err(|e| MemError::Storage(format!("failed to prepare index check: {e}")))?;

            for (&blob_id, rep_path) in &unique {
                let already_indexed = exists_stmt
                    .exists(params![blob_id.to_hex()])
                    .map_err(|e| MemError::Storage(format!("failed to check chunk_index: {e}")))?;
                if already_indexed {
                    stats.blobs_cache_hit += 1;
                    continue;
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

        // Step 2: Parse Markdown and compute embeddings completely outside the SQLite lock.
        let mut parsed_blobs: Vec<(BlobId, ParsedWikiDocument)> = Vec::new();
        let mut all_chunk_texts: Vec<String> = Vec::new();
        let mut chunk_counts: Vec<usize> = Vec::new();

        for (blob_id, path, bytes) in missing_raw {
            let Ok(text) = std::str::from_utf8(&bytes) else {
                // Skip non-UTF-8 binary blobs gracefully.
                continue;
            };
            let parsed = parse_wiki_document(&path, text, self.config.chunk_max_chars);
            chunk_counts.push(parsed.chunks.len());
            for chunk in &parsed.chunks {
                all_chunk_texts.push(chunk.chunk_text.clone());
            }
            parsed_blobs.push((blob_id, parsed));
        }

        if parsed_blobs.is_empty() {
            return Ok(stats);
        }

        let flat_embeddings = if all_chunk_texts.is_empty() {
            None
        } else {
            self.total_embedding_calls
                .fetch_add(1, AtomicOrdering::Relaxed);
            stats.embedding_batches_called += 1;
            self.embedder.embed_batch(&all_chunk_texts)?
        };

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
                     (blob_id, chunk_seq, title, aliases, tags, heading_path, chunk_text, raw_snippet, start_line, end_line, embedding)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
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

            for item in &pending {
                let hex = item.blob_id.to_hex();
                let already = exists_stmt
                    .exists(params![hex])
                    .map_err(|e| MemError::Storage(format!("failed tx existence check: {e}")))?;
                if already {
                    continue;
                }

                let aliases_joined = item.parsed.aliases.join(" | ");
                let tags_joined = item.parsed.tags.join(" ");
                let fts_title = augment_fts_text(&item.parsed.title);
                let fts_aliases = augment_fts_text(&aliases_joined);
                let fts_tags = augment_fts_text(&tags_joined);

                for (idx, chunk) in item.parsed.chunks.iter().enumerate() {
                    let emb_bytes = item
                        .embeddings
                        .as_ref()
                        .and_then(|vecs| vecs.get(idx))
                        .map(|v| encode_f32_vec(v));

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
                        ])
                        .map_err(|e| {
                            MemError::Storage(format!("failed to insert chunk_index: {e}"))
                        })?;

                    let fts_heading = augment_fts_text(&chunk.heading_path);
                    let fts_body = augment_fts_text(&chunk.chunk_text);

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

                stats.blobs_newly_indexed += 1;
            }

            Ok(())
        })?;

        Ok(stats)
    }

    /// Executes branch-scoped hybrid search: BM25 + Vector + 1-hop Wiki Graph RRF fusion.
    pub fn search(&self, req: &SearchRequest) -> Result<Vec<SearchHit>> {
        if req.top_k == 0 || req.query.trim().is_empty() {
            return Ok(Vec::new());
        }

        // 1. Resolve strictly active files for the target revision (no cross-branch leakage).
        let active_entries = self.vcs.list(&req.revision, req.path_prefix.as_ref())?;
        if active_entries.is_empty() {
            return Ok(Vec::new());
        }

        let pairs: Vec<(VirtualPath, BlobId)> = active_entries
            .iter()
            .map(|(p, b, _)| (p.clone(), *b))
            .collect();
        self.index_unique_blobs(&pairs)?;

        // Group active paths by BlobId and preserve metadata per path.
        let mut paths_by_blob: HashMap<BlobId, Vec<VirtualPath>> = HashMap::new();
        let mut meta_by_path: HashMap<VirtualPath, (BlobId, MemoryMetadata)> = HashMap::new();
        for (path, blob_id, metadata) in active_entries {
            paths_by_blob.entry(blob_id).or_default().push(path.clone());
            meta_by_path.insert(path, (blob_id, metadata));
        }

        // 2. Embed query outside the SQLite lock.
        let query_vec = self.embedder.embed_query(&req.query)?;

        // 3. Query FTS5, active embeddings, chunk titles/aliases, and active blob_links inside one lock.
        let active_data = self.vcs.store().with_conn(|conn| {
            load_active_search_data(
                conn,
                &paths_by_blob,
                &req.query,
                query_vec.as_deref(),
                (req.top_k * 5).max(50),
            )
        })?;

        // 4. Build branch-scoped Wiki target resolver (mapping normalized path, stem, title, aliases -> paths).
        let mut wiki_resolver: HashMap<String, Vec<VirtualPath>> = HashMap::new();
        let mut title_by_blob: HashMap<BlobId, String> = HashMap::new();

        for (blob_id, header) in &active_data.headers {
            title_by_blob.insert(*blob_id, header.title.clone());
            if let Some(paths) = paths_by_blob.get(blob_id) {
                for path in paths {
                    register_resolver_keys(&mut wiki_resolver, path, &header.title, &header.aliases);
                }
            }
        }

        // 5. Build branch-scoped outlinks, backlinks, and shared entity index.
        let mut outlinks_map: HashMap<VirtualPath, BTreeSet<VirtualPath>> = HashMap::new();
        let mut backlinks_map: HashMap<VirtualPath, BTreeSet<VirtualPath>> = HashMap::new();
        let mut entity_nodes: HashMap<String, BTreeSet<VirtualPath>> = HashMap::new();

        for link in &active_data.links {
            let Some(source_paths) = paths_by_blob.get(&link.source_blob_id) else {
                continue;
            };

            match link.link_kind.as_str() {
                "wikilink" => {
                    if let Some(targets) = resolve_wiki_target(&wiki_resolver, &link.target_ref) {
                        for src in source_paths {
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

        // 6. Rank Channel 1 (FTS5 + CJK/lexical fallback) by VirtualPath.
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
            for (blob_id, chunk_seq) in &active_data.lexical_fallback {
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

        // 7. Rank Channel 2 (Vector Cosine Similarity) by VirtualPath.
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

        // 9. Channel 3: Obsidian 1-Hop Wiki Graph Expansion (Outlinks + Backlinks + Entity hops).
        let mut graph_scores: HashMap<VirtualPath, f32> = HashMap::new();
        if self.config.enable_wiki_graph_expansion {
            let seed_limit = self.config.seed_top_m.max(1);
            for (seed_path, seed_score) in seed_scores.iter().take(seed_limit) {
                let hop_delta = *seed_score * self.config.graph_hop_decay;
                if hop_delta <= 0.0 {
                    continue;
                }
                if let Some(outs) = outlinks_map.get(seed_path) {
                    for target in outs {
                        if target != seed_path {
                            *graph_scores.entry(target.clone()).or_insert(0.0) += hop_delta;
                            candidate_paths.insert(target.clone());
                        }
                    }
                }
                if let Some(backs) = backlinks_map.get(seed_path) {
                    for source in backs {
                        if source != seed_path {
                            *graph_scores.entry(source.clone()).or_insert(0.0) += hop_delta;
                            candidate_paths.insert(source.clone());
                        }
                    }
                }
                // Entity-triple 1-hop propagation at 0.7x wikilink decay.
                if let Some((seed_blob, _)) = meta_by_path.get(seed_path) {
                    for link in &active_data.links {
                        if link.source_blob_id == *seed_blob && link.link_kind == "entity_rel" {
                            for entity_key in [
                                normalize_lookup_key(&link.target_ref),
                                normalize_lookup_key(&link.source_entity),
                            ] {
                                if let Some(neighbors) = entity_nodes.get(&entity_key) {
                                    for neighbor in neighbors {
                                        if neighbor != seed_path {
                                            *graph_scores.entry(neighbor.clone()).or_insert(0.0) +=
                                                hop_delta * 0.7;
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

        // 10. Combine scores with importance boost and select top_k before fetching snippets.
        struct ScoredCandidate {
            path: VirtualPath,
            blob_id: BlobId,
            chunk_seq: usize,
            score: f32,
            bm25_rank: Option<usize>,
            vector_rank: Option<usize>,
            graph_score: f32,
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
            let total_score = (bm25_part + vec_part + graph_part) * importance_boost;

            let best_chunk_seq = bm25_entry
                .map(|(_, seq)| seq)
                .or_else(|| vec_entry.map(|(_, seq)| seq))
                .unwrap_or(0);

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
        scored.truncate(req.top_k);

        // 11. Fetch chunk snippets for the winning top_k candidates.
        let chunk_keys: Vec<(BlobId, usize)> = scored
            .iter()
            .map(|c| (c.blob_id, c.chunk_seq))
            .collect();
        let chunk_details = self
            .vcs
            .store()
            .with_conn(|conn| fetch_chunk_details(conn, &chunk_keys))?;

        let mut results = Vec::with_capacity(scored.len());
        for cand in scored {
            let detail = chunk_details
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

            let outlinks = outlinks_map
                .get(&cand.path)
                .map(|s| s.iter().cloned().collect())
                .unwrap_or_default();
            let backlinks = backlinks_map
                .get(&cand.path)
                .map(|s| s.iter().cloned().collect())
                .unwrap_or_default();

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

        Ok(results)
    }

    /// Compiles `/core` memories for `rev` within `max_tokens` in `< 5ms` without LLM calls.
    pub fn compile_core(
        &self,
        rev: &Revision,
        max_tokens: usize,
    ) -> Result<CompiledCoreContext> {
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
}

struct ActiveBlobHeader {
    title: String,
    aliases: Vec<String>,
}

struct FtsHit {
    blob_id: BlobId,
    chunk_seq: usize,
    score: f64,
}

struct ActiveLinkRow {
    source_blob_id: BlobId,
    link_kind: String,
    source_entity: String,
    target_ref: String,
}

struct ActiveSearchData {
    headers: Vec<(BlobId, ActiveBlobHeader)>,
    fts_hits: Vec<FtsHit>,
    lexical_fallback: Vec<(BlobId, usize)>,
    vec_hits: Vec<(BlobId, usize, f32)>,
    links: Vec<ActiveLinkRow>,
}

#[derive(Clone)]
struct ChunkDetail {
    title: String,
    heading_path: String,
    raw_snippet: String,
    start_line: usize,
    end_line: usize,
}

fn load_active_search_data(
    conn: &Connection,
    paths_by_blob: &HashMap<BlobId, Vec<VirtualPath>>,
    query: &str,
    query_vec: Option<&[f32]>,
    fts_limit: usize,
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
        // 1. Load headers (title + aliases from chunk_seq = 0) for wiki link resolution.
        let mut headers = Vec::new();
        {
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
                headers.push((BlobId::from_hex(&hex)?, ActiveBlobHeader { title, aliases }));
            }
        }

        // 2. Run FTS5 multi-column weighted BM25 query filtered strictly by _mem_active_blobs.
        let mut fts_hits = Vec::new();
        if let Some(fts_query) = build_fts5_query(query) {
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
                .query_map(params![fts_query, i64::try_from(fts_limit).unwrap_or(100)], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, i64>(1)?,
                        r.get::<_, f64>(2)?,
                    ))
                })
                .map_err(|e| MemError::Storage(format!("failed to execute FTS5 query: {e}")))?;
            for row in rows {
                let (hex, seq, score) =
                    row.map_err(|e| MemError::Storage(format!("failed to decode FTS5 row: {e}")))?;
                fts_hits.push(FtsHit {
                    blob_id: BlobId::from_hex(&hex)?,
                    chunk_seq: usize::try_from(seq.max(0)).unwrap_or(0),
                    score,
                });
            }
        }

        // 3. Fast SQL-filtered lexical fallback only when FTS5 produced no hits (e.g. single CJK char or partial symbol).
        let mut lexical_fallback = Vec::new();
        if fts_hits.is_empty() && !query.trim().is_empty() {
            let mut stmt = conn
                .prepare_cached(
                    "SELECT c.blob_id, c.chunk_seq, c.title, c.aliases, c.tags, c.heading_path, c.chunk_text
                     FROM chunk_index AS c
                     INNER JOIN _mem_active_blobs AS a ON a.blob_id = c.blob_id
                     WHERE INSTR(LOWER(c.title), LOWER(?1)) > 0
                        OR INSTR(LOWER(c.aliases), LOWER(?1)) > 0
                        OR INSTR(LOWER(c.tags), LOWER(?1)) > 0
                        OR INSTR(LOWER(c.chunk_text), LOWER(?1)) > 0",
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
                    MemError::Storage(format!("failed to query active chunks for lexical scan: {e}"))
                })?;

            let mut scored_lex: Vec<(BlobId, usize, f32)> = Vec::new();
            for row in rows {
                let (hex, seq, title, aliases, tags, heading, body) = row.map_err(|e| {
                    MemError::Storage(format!("failed to decode lexical fallback row: {e}"))
                })?;
                let lex_score = score_lexical_match(query, &title, &aliases, &tags, &heading, &body);
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

        // 4. Score active embeddings zero-allocation directly from SQLite BLOB slices.
        let mut vec_hits = Vec::new();
        if let Some(q_vec) = query_vec {
            let mut stmt = conn
                .prepare_cached(
                    "SELECT c.blob_id, c.chunk_seq, c.embedding
                     FROM chunk_index AS c
                     INNER JOIN _mem_active_blobs AS a ON a.blob_id = c.blob_id
                     WHERE c.embedding IS NOT NULL",
                )
                .map_err(|e| {
                    MemError::Storage(format!("failed to prepare embedding query: {e}"))
                })?;
            let mut rows = stmt.query([]).map_err(|e| {
                MemError::Storage(format!("failed to query active embeddings: {e}"))
            })?;
            let mut vec_best_by_blob: HashMap<BlobId, (usize, f32)> = HashMap::new();
            while let Some(row) = rows
                .next()
                .map_err(|e| MemError::Storage(format!("failed to step embedding row: {e}")))?
            {
                let vec_bytes = row
                    .get_ref(2)
                    .map_err(|e| MemError::Storage(format!("failed to read embedding ref: {e}")))?
                    .as_bytes()
                    .map_err(|e| MemError::Storage(format!("invalid embedding blob type: {e}")))?;
                let sim = cosine_similarity_le_bytes(q_vec, vec_bytes)?;
                if sim <= 0.0 {
                    continue;
                }
                let hex = row
                    .get_ref(0)
                    .map_err(|e| MemError::Storage(format!("failed to read blob_id ref: {e}")))?
                    .as_str()
                    .map_err(|e| MemError::Storage(format!("invalid blob_id ref type: {e}")))?;
                let seq_i64 = row
                    .get::<_, i64>(1)
                    .map_err(|e| MemError::Storage(format!("failed to read chunk_seq: {e}")))?;
                let blob_id = BlobId::from_hex(hex)?;
                let chunk_seq = usize::try_from(seq_i64.max(0)).unwrap_or(0);
                vec_best_by_blob
                    .entry(blob_id)
                    .and_modify(|cur| {
                        if sim > cur.1 {
                            *cur = (chunk_seq, sim);
                        }
                    })
                    .or_insert((chunk_seq, sim));
            }
            vec_hits = vec_best_by_blob
                .into_iter()
                .map(|(b, (seq, s))| (b, seq, s))
                .collect();
            vec_hits.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap_or(Ordering::Equal));
            vec_hits.truncate(fts_limit);
        }

        // 5. Load branch-scoped graph edges from blob_links.
        let mut links = Vec::new();
        {
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
                .map_err(|e| MemError::Storage(format!("failed to query active blob_links: {e}")))?;
            for row in rows {
                let (hex, kind, source_entity, target_ref) = row
                    .map_err(|e| MemError::Storage(format!("failed to decode link row: {e}")))?;
                links.push(ActiveLinkRow {
                    source_blob_id: BlobId::from_hex(&hex)?,
                    link_kind: kind,
                    source_entity,
                    target_ref,
                });
            }
        }

        Ok(ActiveSearchData {
            headers,
            fts_hits,
            lexical_fallback,
            vec_hits,
            links,
        })
    })();

    let _ = conn.execute("DELETE FROM _mem_active_blobs", []);
    result
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
        let row = stmt
            .query_row(params![blob_id.to_hex(), seq_i64], |r| {
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

fn build_fts5_query(query: &str) -> Option<String> {
    let mut tokens: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();

    for raw in query.split(|c: char| !c.is_alphanumeric() && c != '_') {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            continue;
        }
        // FTS5 `unicode61` splits `_` into adjacent tokens, so quoting `"BUSY_TIMEOUT"`
        // forms an exact phrase `"busy timeout"` that matches cleanly.
        let lower = trimmed.to_lowercase();
        if seen.insert(lower) {
            let escaped = trimmed.replace('"', "\"\"");
            tokens.push(format!("\"{escaped}\""));
        }

        // Also emit CJK bigrams for multi-character CJK runs so substrings match via FTS5.
        let mut cjk_run: Vec<char> = Vec::new();
        for ch in trimmed.chars() {
            if is_cjk(ch) {
                cjk_run.push(ch);
            } else {
                if cjk_run.len() >= 2 {
                    for pair in cjk_run.windows(2) {
                        let bg: String = pair.iter().collect();
                        if seen.insert(bg.clone()) {
                            tokens.push(format!("\"{bg}\""));
                        }
                    }
                }
                cjk_run.clear();
            }
        }
        if cjk_run.len() >= 2 {
            for pair in cjk_run.windows(2) {
                let bg: String = pair.iter().collect();
                if seen.insert(bg.clone()) {
                    tokens.push(format!("\"{bg}\""));
                }
            }
        }
    }

    if tokens.is_empty() {
        None
    } else {
        Some(tokens.join(" OR "))
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
