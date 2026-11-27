use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::Utc;
use mem_core::{MemoryMetadata, MemoryNode, MemoryTier, Result, Revision, VirtualPath};
use mem_search::{
    compile_core, consolidate, ingest_knowledge, is_linkworthy_anchor, mark_rewind_events_mined,
    mine_branch, mine_commit_history, mine_rewind_pitfalls, parse_wiki_document, record_node_read,
    ConsolidateLlmConfig, ConsolidateStatus, DeterministicHashEmbedder, EmbeddingProvider,
    IngestRequest, IngestRole, MemSearchEngine, ProviderBackend, SearchConfig, SearchRequest,
};
use mem_store::SqliteStore;
use mem_vcs::{MemVcs, VersionControlEngine};

fn vp(s: &str) -> VirtualPath {
    VirtualPath::new(s).expect("valid virtual path")
}

fn node_at(
    path: &str,
    body: &str,
    importance: f32,
    created_at_ms: i64,
    ttl_seconds: Option<u64>,
) -> MemoryNode {
    MemoryNode::new(vp(path), body.as_bytes().to_vec()).with_metadata(MemoryMetadata {
        content_type: "text/markdown".to_string(),
        importance,
        tags: Vec::new(),
        created_at_ms,
        updated_at_ms: created_at_ms,
        ttl_seconds,
    })
}

fn new_bm25_engine() -> MemSearchEngine {
    let store = SqliteStore::open_in_memory().unwrap();
    let vcs = MemVcs::new(store);
    vcs.init().unwrap();
    MemSearchEngine::new_bm25_only(vcs)
}

struct QueryCountingEmbedder {
    inner: DeterministicHashEmbedder,
    query_calls: Arc<AtomicUsize>,
    batch_calls: Arc<AtomicUsize>,
}

impl QueryCountingEmbedder {
    fn new(dim: usize) -> Self {
        Self {
            inner: DeterministicHashEmbedder::new(dim),
            query_calls: Arc::new(AtomicUsize::new(0)),
            batch_calls: Arc::new(AtomicUsize::new(0)),
        }
    }
}

impl EmbeddingProvider for QueryCountingEmbedder {
    fn provider_name(&self) -> &str {
        "counting-hash"
    }

    fn embed_batch(&self, texts: &[String]) -> Result<Option<Vec<Vec<f32>>>> {
        self.batch_calls.fetch_add(1, Ordering::Relaxed);
        self.inner.embed_batch(texts)
    }

    fn embed_query(&self, query: &str) -> Result<Option<Vec<f32>>> {
        self.query_calls.fetch_add(1, Ordering::Relaxed);
        self.inner.embed_query(query)
    }
}

#[test]
fn ttl_seconds_enforced_on_live_views_and_deterministic_at_commit() {
    let engine = new_bm25_engine();
    let vcs = engine.vcs();
    let now_ms = Utc::now().timestamp_millis();

    // Immediately expired node (created 60s ago with 5s TTL) vs freshly created 1s TTL node vs permanent node.
    vcs.write(
        "main",
        node_at(
            "/core/expired-core.md",
            "STALE_SECRET_CORE_RULE should disappear when expired.",
            1.0,
            now_ms - 60_000,
            Some(5),
        ),
    )
    .unwrap();
    vcs.write(
        "main",
        node_at(
            "/semantic/short-ttl.md",
            "# Short Window\nSHORT_TTL_MARKER valid only at initial commit time.",
            0.9,
            now_ms,
            Some(1),
        ),
    )
    .unwrap();
    vcs.write(
        "main",
        node_at(
            "/core/live-core.md",
            "--- \nowner: \"team-storage\"\n---\n\n# Live Core Rule\nPERMANENT_CORE_RULE stays compiled.\n\n\n## Details\nKeep WAL active.",
            0.9,
            now_ms,
            None,
        ),
    )
    .unwrap();
    let c1 = vcs.commit("main", "tester", "seed ttl nodes").unwrap();

    // Wait just over 1s so /semantic/short-ttl.md expires relative to `now`, while `c1.created_at_ms` remains before expiration.
    std::thread::sleep(Duration::from_millis(1150));

    // Live views hide both expired nodes.
    let live_ws = Revision::BranchWorkspace("main".to_string());
    let live_head = Revision::BranchHead("main".to_string());
    assert!(vcs
        .read(&live_ws, &vp("/core/expired-core.md"))
        .unwrap()
        .is_none());
    assert!(vcs
        .read(&live_ws, &vp("/semantic/short-ttl.md"))
        .unwrap()
        .is_none());
    assert!(vcs
        .read(&live_head, &vp("/semantic/short-ttl.md"))
        .unwrap()
        .is_none());

    let listed = vcs.list(&live_ws, None).unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].0, vp("/core/live-core.md"));

    // Historical commit revision stays deterministic relative to commit timestamp.
    assert!(vcs
        .read(&Revision::Commit(c1), &vp("/semantic/short-ttl.md"))
        .unwrap()
        .is_some());
    assert!(vcs
        .read(&Revision::Commit(c1), &vp("/core/expired-core.md"))
        .unwrap()
        .is_none());

    // Search and compile_core omit expired nodes and strip YAML frontmatter.
    let hits = engine
        .search(&SearchRequest::new(live_ws.clone(), "SHORT_TTL_MARKER"))
        .unwrap();
    assert!(hits.is_empty());

    let compiled = compile_core(vcs, &live_head, 500).unwrap();
    assert!(!compiled.rendered.contains("STALE_SECRET_CORE_RULE"));
    assert!(!compiled.rendered.contains("owner: \"team-storage\""));
    assert!(compiled.rendered.contains("PERMANENT_CORE_RULE"));
}

#[test]
fn core_compiler_partially_fits_leading_subsections_within_budget() {
    let engine = new_bm25_engine();
    let vcs = engine.vcs();
    let now_ms = Utc::now().timestamp_millis();

    let big_doc = format!(
        "---\ntitle: \"Architecture Rules\"\n---\n\n# Architecture Rules\n\n## Section One\nALWAYS_KEEP_ALPHA.\n\n## Section Two Overflow\n{}\n",
        "VERY_LONG_OVERFLOW_WORD ".repeat(220)
    );
    vcs.write(
        "main",
        node_at("/core/rules.md", &big_doc, 1.0, now_ms, None),
    )
    .unwrap();

    let compiled = compile_core(vcs, &Revision::BranchWorkspace("main".into()), 140).unwrap();
    assert!(compiled.rendered.contains("ALWAYS_KEEP_ALPHA"));
    assert!(!compiled.rendered.contains("title: \"Architecture Rules\""));
    assert!(compiled.estimated_tokens <= 150);
}

#[test]
fn query_embedding_lru_and_snapshot_invalidation_keep_branches_isolated() {
    let store = SqliteStore::open_in_memory().unwrap();
    let vcs = MemVcs::new(store);
    vcs.init().unwrap();
    let embedder = Arc::new(QueryCountingEmbedder::new(64));
    let engine = MemSearchEngine::new(vcs.clone(), embedder.clone(), SearchConfig::default());
    let now_ms = Utc::now().timestamp_millis();

    vcs.write(
        "main",
        node_at(
            "/semantic/wal.md",
            "# SQLite WAL\nEnable busy_timeout for concurrency.",
            0.8,
            now_ms,
            None,
        ),
    )
    .unwrap();
    vcs.commit("main", "dev", "base").unwrap();

    // First search populates both BranchSearchSnapshot and query_vec_cache.
    let h1 = engine
        .search(&SearchRequest::new(
            Revision::BranchWorkspace("main".into()),
            "busy_timeout concurrency",
        ))
        .unwrap();
    assert_eq!(h1.len(), 1);
    assert_eq!(embedder.query_calls.load(Ordering::Relaxed), 1);

    // Repeating the same query uses the cached query embedding without calling embed_query again.
    let h2 = engine
        .search(&SearchRequest::new(
            Revision::BranchWorkspace("main".into()),
            "busy_timeout concurrency",
        ))
        .unwrap();
    assert_eq!(h2.len(), 1);
    assert_eq!(embedder.query_calls.load(Ordering::Relaxed), 1);

    // Feature branch write stays invisible on `main`, yet invalidates `feature` snapshot cleanly.
    vcs.create_branch("feature", &Revision::BranchHead("main".into()))
        .unwrap();
    vcs.write(
        "feature",
        node_at(
            "/semantic/feature-only.md",
            "# Feature Cache Rule\nFEATURE_ISOLATION_TOKEN applies only on feature branch.",
            0.9,
            now_ms,
            None,
        ),
    )
    .unwrap();

    let feat_hits = engine
        .search(&SearchRequest::new(
            Revision::BranchWorkspace("feature".into()),
            "FEATURE_ISOLATION_TOKEN",
        ))
        .unwrap();
    assert_eq!(feat_hits[0].path, vp("/semantic/feature-only.md"));
    assert_eq!(feat_hits[0].bm25_rank, Some(1));

    let main_hits = engine
        .search(&SearchRequest::new(
            Revision::BranchWorkspace("main".into()),
            "FEATURE_ISOLATION_TOKEN",
        ))
        .unwrap();
    assert!(!main_hits
        .iter()
        .any(|h| h.path == vp("/semantic/feature-only.md")));
    assert!(main_hits.iter().all(|h| h.bm25_rank.is_none()));
}

#[test]
fn fts5_column_decoupling_chunk_merge_and_hub_penalized_graph_expansion() {
    let cfg = SearchConfig {
        chunk_max_chars: 140,
        ..Default::default()
    };
    let store = SqliteStore::open_in_memory().unwrap();
    let vcs = MemVcs::new(store);
    vcs.init().unwrap();
    let engine = MemSearchEngine::new(vcs.clone(), Arc::new(mem_search::NoopEmbedder), cfg);
    let now_ms = Utc::now().timestamp_millis();

    // Multi-chunk doc: chunk 0 is unrelated intro; chunks 1 & 2 discuss BUSY_TIMEOUT_TARGET adjacently.
    let long_doc = format!(
        "# Long Manual\n\n## Intro\n{}\n\n## Concurrency\nBUSY_TIMEOUT_TARGET first paragraph part.\n\nBUSY_TIMEOUT_TARGET second adjacent detail.\n",
        "Introductory context sentence without target keywords. ".repeat(3)
    );
    vcs.write(
        "main",
        node_at("/semantic/long-manual.md", &long_doc, 0.8, now_ms, None),
    )
    .unwrap();

    // Seed node -> links to DirectNeighbor (1-to-1) AND NoisyHub (in_degree = 20 from dummy leaves).
    vcs.write(
        "main",
        node_at(
            "/semantic/seed-topic.md",
            "# Seed Topic\nUNIQUE_GRAPH_SEED\nLinks to [[/semantic/direct-neighbor.md]] and [[/semantic/noisy-hub.md]].",
            0.9,
            now_ms,
            None,
        ),
    )
    .unwrap();
    vcs.write(
        "main",
        node_at(
            "/semantic/direct-neighbor.md",
            "# Direct Neighbor\nDedicated architectural invariant.",
            0.8,
            now_ms,
            None,
        ),
    )
    .unwrap();
    vcs.write(
        "main",
        node_at(
            "/semantic/noisy-hub.md",
            "# Noisy Hub\nGeneric index note.",
            0.8,
            now_ms,
            None,
        ),
    )
    .unwrap();
    for i in 0..20 {
        vcs.write(
            "main",
            node_at(
                &format!("/semantic/leaves/leaf-{i:02}.md"),
                &format!("# Leaf {i}\nRefers to [[/semantic/noisy-hub.md]]."),
                0.5,
                now_ms,
                None,
            ),
        )
        .unwrap();
    }
    vcs.commit("main", "dev", "seed graph and chunks").unwrap();

    let rev = Revision::BranchWorkspace("main".into());

    // 1. FTS5 synthetic label decoupling: querying "Aliases" or "Section" does NOT false-match.
    let meta_noise = engine
        .search(&SearchRequest::new(rev.clone(), "Aliases Section Document"))
        .unwrap();
    assert!(meta_noise.is_empty());

    // 2. Chunk-level RRF + adjacent chunk merge surfaces matching body snippet, not intro chunk 0.
    let chunk_hits = engine
        .search(&SearchRequest::new(rev.clone(), "BUSY_TIMEOUT_TARGET"))
        .unwrap();
    assert_eq!(chunk_hits[0].path, vp("/semantic/long-manual.md"));
    assert!(chunk_hits[0].snippet.contains("first paragraph"));
    assert!(chunk_hits[0].snippet.contains("second adjacent"));

    // 3. Hub-penalized 1-hop expansion ranks direct 1-to-1 neighbor above 20-in-degree hub.
    let graph_hits = engine
        .search(&SearchRequest::new(rev, "UNIQUE_GRAPH_SEED"))
        .unwrap();
    let direct_pos = graph_hits
        .iter()
        .position(|h| h.path == vp("/semantic/direct-neighbor.md"))
        .expect("direct neighbor included");
    let hub_pos = graph_hits
        .iter()
        .position(|h| h.path == vp("/semantic/noisy-hub.md"))
        .expect("hub neighbor included");
    assert!(direct_pos < hub_pos);
}

#[test]
fn structure_aware_markdown_chunker_preserves_fences_and_tables_and_skips_punctuation() {
    let md = "\
# Build Notes

---

```rust
fn alpha_one() { println!(\"one\"); }
fn alpha_two() { println!(\"two\"); }
fn alpha_three() { println!(\"three\"); }
fn alpha_four() { println!(\"four\"); }
```

## Matrix

| Service | TimeoutMs | Retries |
| --- | --- | --- |
| checkout | 5000 | 3 |
| ledger | 3000 | 2 |
| gateway | 2500 | 4 |
| notifier | 1500 | 1 |
";

    let parsed = parse_wiki_document(&vp("/semantic/build.md"), md, 95);
    assert!(parsed
        .chunks
        .iter()
        .all(|c| c.raw_snippet.chars().any(|ch| ch.is_alphanumeric())));
    assert!(!parsed.chunks.iter().any(|c| c.raw_snippet.trim() == "---"));

    let code_chunks: Vec<_> = parsed
        .chunks
        .iter()
        .filter(|c| c.raw_snippet.contains("fn alpha_"))
        .collect();
    assert!(code_chunks.len() >= 2);
    for (idx, chunk) in code_chunks.iter().enumerate() {
        assert!(chunk.raw_snippet.contains("```rust"));
        assert!(chunk.raw_snippet.trim_end().ends_with("```"));
        if idx > 0 {
            assert!(chunk.raw_snippet.trim_start().starts_with("```rust"));
        }
    }

    let table_chunks: Vec<_> = parsed
        .chunks
        .iter()
        .filter(|c| c.heading_path.contains("## Matrix"))
        .collect();
    assert!(table_chunks.len() >= 2);
    for chunk in &table_chunks {
        assert!(chunk
            .raw_snippet
            .contains("| Service | TimeoutMs | Retries |"));
    }
}

#[test]
fn bot_ingest_redacts_secrets_suppresses_near_duplicates_and_filters_hub_anchors() {
    let engine = new_bm25_engine();
    let vcs = engine.vcs();
    let now_ms = Utc::now().timestamp_millis();

    // Seed ubiquitous title "Overview" in > 60% of branch notes so IDF rejects it as a hub anchor,
    // alongside specific anchor "PgBouncer Pool".
    for i in 0..4 {
        vcs.write(
            "main",
            node_at(
                &format!("/semantic/overview-{i}.md"),
                "# Overview\nGeneral service notes about Overview overview overview.",
                0.7,
                now_ms,
                None,
            ),
        )
        .unwrap();
    }
    vcs.write(
        "main",
        node_at(
            "/semantic/pgbouncer-pool.md",
            "---\ntitle: \"PgBouncer Pool\"\n---\n# PgBouncer Pool\nConnection pooling config.",
            0.85,
            now_ms,
            None,
        ),
    )
    .unwrap();
    vcs.commit("main", "dev", "seed anchors").unwrap();

    assert!(!is_linkworthy_anchor("Overview", 4, 5));
    assert!(is_linkworthy_anchor("PgBouncer Pool", 1, 5));

    let fake_openai = ["sk-", "proj-abcdefghijklmnopqrstuvwxyz123456"].concat();
    let out1 = ingest_knowledge(
        &engine,
        &IngestRequest::new(
            IngestRole::Bot,
            format!(
                "Observed PgBouncer Pool saturation in checkout worker at 2026-11-26T10:00:01Z while using key {fake_openai}."
            ),
        )
        .with_title("Worker Pool Saturation"),
        None,
    )
    .unwrap();
    assert!(!out1.deduplicated);
    assert!(out1.secrets_redacted >= 1);
    assert!(out1.markdown.contains("[REDACTED_SECRET]"));
    assert!(!out1.markdown.contains(&fake_openai));
    assert!(out1.markdown.contains("[[PgBouncer Pool]]"));
    assert!(!out1.markdown.contains("[[Overview]]"));

    // Second near-identical Bot ingest with different timestamp triggers SimHash deduplication.
    let out2 = ingest_knowledge(
        &engine,
        &IngestRequest::new(
            IngestRole::Bot,
            "Observed PgBouncer Pool saturation in checkout worker at 2026-11-26T10:05:09Z while using key [REDACTED_SECRET].",
        )
        .with_title("Another Worker Pool Log"),
        None,
    )
    .unwrap();
    assert!(out2.deduplicated);
    assert_eq!(out2.path, out1.path);
}

#[test]
fn binary_quantization_two_stage_vector_search_scales_above_256_chunks() {
    let store = SqliteStore::open_in_memory().unwrap();
    let vcs = MemVcs::new(store.clone());
    vcs.init().unwrap();

    let cfg = SearchConfig {
        bm25_weight: 0.0,
        enable_wiki_graph_expansion: false,
        graph_weight: 0.0,
        ..Default::default()
    };
    let embedder = Arc::new(DeterministicHashEmbedder::new(128));
    let engine = MemSearchEngine::new(vcs.clone(), embedder, cfg);
    let now_ms = Utc::now().timestamp_millis();

    // Seed 310 filler notes + 1 highly aligned target note so BQ two-stage scoring engages (> 256 chunks).
    for i in 0..310 {
        vcs.write(
            "main",
            node_at(
                &format!("/semantic/batch/doc-{i:03}.md"),
                &format!("# Batch Note {i}\nRoutine telemetry observation number {i} for background storage."),
                0.5,
                now_ms,
                None,
            ),
        )
        .unwrap();
    }
    vcs.write(
        "main",
        node_at(
            "/semantic/quantum-wal-target.md",
            "# Quantum WAL Target\nDeterministic binary quantization cosine acceleration for SQLite WAL frames.",
            0.9,
            now_ms,
            None,
        ),
    )
    .unwrap();
    vcs.commit("main", "dev", "seed 311 docs").unwrap();

    let hits = engine
        .search(&SearchRequest::new(
            Revision::BranchWorkspace("main".into()),
            "Deterministic binary quantization cosine acceleration SQLite WAL",
        ))
        .unwrap();
    assert_eq!(hits[0].path, vp("/semantic/quantum-wal-target.md"));
    assert!(hits[0].vector_rank.is_some());

    let (bq_count, max_emb_len): (i64, i64) = store
        .with_conn(|conn| {
            conn.query_row(
                "SELECT COUNT(*), COALESCE(MAX(length(embedding)), 0) FROM chunk_index WHERE embedding_bq IS NOT NULL",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .map_err(|e| mem_core::MemError::Storage(e.to_string()))
        })
        .unwrap();
    assert!(bq_count >= 311);
    assert_eq!(max_emb_len, 128 * 4);
}

#[test]
fn s6_consolidate_scrubs_secrets_weaves_links_and_blocks_anchor_stripping() {
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;
    use std::thread;

    let engine = new_bm25_engine();
    let vcs = engine.vcs();
    let now_ms = Utc::now().timestamp_millis();

    vcs.write(
        "main",
        node_at(
            "/semantic/target-anchor.md",
            "# Target Anchor\nActive invariant reference.",
            0.85,
            now_ms,
            None,
        ),
    )
    .unwrap();
    vcs.write(
        "main",
        node_at(
            "/semantic/guarded-note.md",
            "# Guarded Note\nAlways verify [[Target Anchor]] before deployment.",
            0.85,
            now_ms,
            None,
        ),
    )
    .unwrap();
    let secret_token = ["ghp_", "1234567890abcdefghijklmnopqrstuvwxyzABCD"].concat();
    vcs.write(
        "main",
        node_at(
            "/episodic/session-s6.md",
            &format!(
                "# Session S6\nUsed token {secret_token} while reviewing Target Anchor pipeline."
            ),
            0.7,
            now_ms,
            None,
        ),
    )
    .unwrap();
    vcs.commit("main", "dev", "seed s6").unwrap();

    let reply = serde_json::json!({
        "notes": [
            {
                "path": "/semantic/distilled-s6.md",
                "content": "# Distilled S6\nReviewed Target Anchor during rollout."
            },
            {
                "path": "/semantic/guarded-note.md",
                "content": "# Guarded Note\nRewritten without any wikilinks."
            }
        ]
    })
    .to_string();

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/v1", listener.local_addr().unwrap());
    let seen_bodies = Arc::new(Mutex::new(Vec::new()));
    let seen_clone = seen_bodies.clone();
    thread::spawn(move || {
        if let Ok((mut stream, _)) = listener.accept() {
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut len = 0usize;
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    len = v.trim().parse().unwrap();
                }
                if line == "\r\n" {
                    break;
                }
            }
            let mut body = vec![0; len];
            reader.read_exact(&mut body).unwrap();
            seen_clone
                .lock()
                .unwrap()
                .push(String::from_utf8(body).unwrap());
            let resp = serde_json::json!({"choices": [{"index": 0, "message": {"role": "assistant", "content": reply}}]}).to_string();
            let head = format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", resp.len());
            stream.write_all(head.as_bytes()).unwrap();
            stream.write_all(resp.as_bytes()).unwrap();
        }
    });

    let cfg = ConsolidateLlmConfig {
        backend: ProviderBackend::Ollama,
        base_url: url,
        model: "mock-llm".into(),
        api_key: None,
    };
    let outcome = consolidate(&engine, &cfg, None, "consolidate/s6-hygiene").unwrap();
    assert_eq!(outcome.status, ConsolidateStatus::Succeeded);
    assert_eq!(outcome.notes_written, 1);
    assert!(outcome.notes_dropped >= 1);

    let prompts = seen_bodies.lock().unwrap();
    assert!(!prompts[0].contains(&secret_token));
    assert!(prompts[0].contains("[REDACTED_SECRET]"));

    let review_rev = Revision::BranchHead("consolidate/s6-hygiene".into());
    let written = vcs
        .read(&review_rev, &vp("/semantic/distilled-s6.md"))
        .unwrap()
        .map(|n| String::from_utf8(n.content).unwrap())
        .unwrap();
    assert!(written.contains("[[Target Anchor]]"));
}

#[test]
fn telemetry_closed_loop_utility_and_branch_isolation() {
    let engine = new_bm25_engine();
    let vcs = engine.vcs();
    let now_ms = Utc::now().timestamp_millis();

    let alpha_path = vp("/semantic/alpha-utility.md");
    let beta_path = vp("/semantic/beta-utility.md");
    let alpha_blob = vcs
        .write_node(
            "main",
            node_at(
                alpha_path.as_str(),
                "# Alpha Utility\nSHARED_UTILITY_TERM checkout circuit breaker guidance.",
                0.7,
                now_ms,
                None,
            ),
        )
        .unwrap();
    let beta_blob = vcs
        .write_node(
            "main",
            node_at(
                beta_path.as_str(),
                "# Beta Utility\nSHARED_UTILITY_TERM checkout circuit breaker guidance.",
                0.85,
                now_ms,
                None,
            ),
        )
        .unwrap();
    let safe_cid = vcs.commit("main", "dev", "seed twins").unwrap();

    // Create a feature branch before telemetry so we verify branch isolation too.
    vcs.create_branch("feature-tel", &Revision::Commit(safe_cid))
        .unwrap();

    // Alpha is read and followed by successful commits; Beta is read then followed by rewind.
    record_node_read(vcs.store(), "main", &alpha_path, &alpha_blob).unwrap();
    vcs.write_node(
        "main",
        node_at(
            "/episodic/good-work.md",
            "Completed task cleanly.",
            0.5,
            now_ms,
            None,
        ),
    )
    .unwrap();
    vcs.commit("main", "dev", "good progress 1").unwrap();
    vcs.write_node(
        "main",
        node_at(
            "/episodic/good-work-2.md",
            "Follow-up verification passed.",
            0.5,
            now_ms,
            None,
        ),
    )
    .unwrap();
    vcs.commit("main", "dev", "good progress 2").unwrap();

    record_node_read(vcs.store(), "main", &beta_path, &beta_blob).unwrap();
    vcs.write_node(
        "main",
        node_at(
            "/episodic/bad-step.md",
            "Bad path to rewind.",
            0.5,
            now_ms,
            None,
        ),
    )
    .unwrap();
    vcs.commit("main", "dev", "bad commit").unwrap();
    vcs.rewind("main", &safe_cid).unwrap();

    // Feature branch search should not see main's telemetry bias, so higher-importance Beta leads.
    let feat_hits = engine
        .search(&SearchRequest::new(
            Revision::BranchWorkspace("feature-tel".into()),
            "SHARED_UTILITY_TERM",
        ))
        .unwrap();
    assert_eq!(feat_hits[0].path, beta_path);
    assert!(feat_hits[0].score > feat_hits[1].score);

    // Main branch ranks Alpha above Beta based on utility feedback.
    let main_hits = engine
        .search(&SearchRequest::new(
            Revision::BranchWorkspace("main".into()),
            "SHARED_UTILITY_TERM",
        ))
        .unwrap();
    assert_eq!(main_hits[0].path, alpha_path);
    assert!(main_hits[0].score > main_hits[1].score);

    // Overwriting Beta with new content resets its telemetry penalty for the new blob.
    let new_beta_blob = vcs
        .write_node(
            "main",
            node_at(
                beta_path.as_str(),
                "# Beta Utility v2\nSHARED_UTILITY_TERM updated verified note.",
                0.7,
                now_ms,
                None,
            ),
        )
        .unwrap();
    record_node_read(vcs.store(), "main", &beta_path, &new_beta_blob).unwrap();
    let mult = vcs
        .store()
        .with_conn(|c| {
            let map = mem_search::load_branch_telemetry(c, "main")?;
            Ok(mem_search::utility_multiplier(
                &map[&beta_path],
                &new_beta_blob,
            ))
        })
        .unwrap();
    assert!(mult >= 1.0);
}

#[test]
fn commit_cochange_and_branch_mining_surface_signals_and_boost_search() {
    let engine = new_bm25_engine();
    let vcs = engine.vcs();
    let now_ms = Utc::now().timestamp_millis();

    let svc = vp("/semantic/billing-svc.md");
    let runbook = vp("/procedural/billing-runbook.md");

    // 4 semantic commits co-modifying billing-svc + billing-runbook (triggers both co-change >= 2 and churn >= 4).
    for step in 1..=4 {
        vcs.write_node(
            "main",
            node_at(
                svc.as_str(),
                &format!(
                    "---\naliases: [\"Billing Service\"]\nentities: [\"BillingService -> uses -> LedgerDB\"]\n---\n# Billing Service v{step}\nCOCHANGE_SEED_TOKEN revision {step}.\nAlso references [[Uncreated Playbook]]."
                ),
                0.85,
                now_ms + step,
                None,
            ),
        )
        .unwrap();
        vcs.write_node(
            "main",
            node_at(
                runbook.as_str(),
                &format!("# Billing Runbook v{step}\nExecute database vacuum step {step} without explicit wikilink."),
                0.8,
                now_ms + step,
                None,
            ),
        )
        .unwrap();
        vcs.commit("main", "dev", &format!("cochange commit {step}"))
            .unwrap();
    }

    // Add a partner sharing the same LedgerDB entity (no wikilink between them) to test missing_links,
    // an expired node, a high-utility episodic promotion candidate, and a rewind event.
    vcs.write_node(
        "main",
        node_at(
            "/semantic/ledger-partner.md",
            "---\nentities: [\"LedgerPartner -> connects -> LedgerDB\"]\n---\n# Ledger Partner\nShared DB architecture.",
            0.75,
            now_ms,
            None,
        ),
    )
    .unwrap();
    vcs.write_node(
        "main",
        node_at(
            "/episodic/expired-ep.md",
            "Old TTL episode.",
            0.5,
            now_ms - 10_000,
            Some(1),
        ),
    )
    .unwrap();
    let promo_path = vp("/episodic/hot-fix-note.md");
    let promo_blob = vcs
        .write_node(
            "main",
            node_at(
                promo_path.as_str(),
                "# Hot Fix Note\nWorkaround fix repeated across incidents.",
                0.7,
                now_ms,
                None,
            ),
        )
        .unwrap();
    let baseline = vcs.commit("main", "dev", "seed mining signals").unwrap();

    for _ in 0..3 {
        record_node_read(vcs.store(), "main", &promo_path, &promo_blob).unwrap();
    }
    vcs.write_node(
        "main",
        node_at(
            "/procedural/broken-tweak.md",
            "# Broken Tweak\nDo not disable foreign_keys.",
            0.8,
            now_ms,
            None,
        ),
    )
    .unwrap();
    vcs.commit("main", "dev", "bad commit to rewind").unwrap();
    vcs.rewind("main", &baseline).unwrap();

    // 1. Co-change boost surfaces billing-runbook even without [[Wikilink]].
    let search_hits = engine
        .search(&SearchRequest::new(
            Revision::BranchWorkspace("main".into()),
            "COCHANGE_SEED_TOKEN",
        ))
        .unwrap();
    assert!(search_hits.iter().any(|h| h.path == runbook));

    // 2. mine_commit_history & mine_branch surface all expected signals.
    let (edges, churn) = mine_commit_history(vcs, "main", 20).unwrap();
    assert!(edges.iter().any(|e| {
        (e.path_a == svc && e.path_b == runbook) || (e.path_a == runbook && e.path_b == svc)
    }));
    assert!(churn.iter().any(|(p, c)| *p == svc && *c >= 4));

    let report = mine_branch(&engine, "main").unwrap();
    assert!(report
        .dangling_links
        .iter()
        .any(|d| d.target_ref == "Uncreated Playbook"));
    assert!(report.missing_links.iter().any(|m| {
        (m.path_a == svc && m.path_b == vp("/semantic/ledger-partner.md"))
            || (m.path_b == svc && m.path_a == vp("/semantic/ledger-partner.md"))
    }));
    assert!(report
        .expired_paths
        .contains(&vp("/episodic/expired-ep.md")));
    assert!(report
        .promotion_candidates
        .iter()
        .any(|p| p.path == promo_path && p.suggested_tier == MemoryTier::Procedural));
    assert_eq!(report.rewind_pitfalls.len(), 1);
    assert!(report.rewind_pitfalls[0]
        .reverted_paths
        .contains(&vp("/procedural/broken-tweak.md")));

    let ids: Vec<i64> = report.rewind_pitfalls.iter().map(|p| p.event_id).collect();
    mark_rewind_events_mined(vcs.store(), &ids).unwrap();
    assert!(mine_rewind_pitfalls(vcs, "main").unwrap().is_empty());
}
