use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use mem_core::{
    MemoryMetadata, MemoryNode, MemoryTier, MergeStrategy, Result, Revision, VirtualPath,
};
use mem_search::{
    compile_core, init_vault_template, ConsolidateLlmConfig, DeterministicHashEmbedder,
    EmbeddingProvider, EmbeddingProviderConfig, IngestRequest, IngestRole, MemConfig,
    MemSearchEngine, OpenAiCompatibleEmbedder, ProviderBackend, SearchConfig, SearchRequest,
};
use mem_store::SqliteStore;
use mem_vcs::{MemVcs, VersionControlEngine};
use tempfile::tempdir;

fn vp(s: &str) -> VirtualPath {
    VirtualPath::new(s).expect("valid virtual path")
}

fn make_node(path: &str, body: &str, importance: f32, tags: Vec<&str>) -> MemoryNode {
    MemoryNode::new(vp(path), body.as_bytes().to_vec()).with_metadata(MemoryMetadata {
        content_type: "text/markdown".to_string(),
        importance,
        tags: tags.into_iter().map(String::from).collect(),
        created_at_ms: 1_700_000_000_000,
        updated_at_ms: 1_700_000_000_000,
        ttl_seconds: None,
    })
}

struct SpyEmbedder {
    inner: DeterministicHashEmbedder,
    batch_calls: Arc<AtomicUsize>,
    texts_embedded: Arc<AtomicUsize>,
}

impl SpyEmbedder {
    fn new() -> Self {
        Self {
            inner: DeterministicHashEmbedder::new(128),
            batch_calls: Arc::new(AtomicUsize::new(0)),
            texts_embedded: Arc::new(AtomicUsize::new(0)),
        }
    }
}

impl EmbeddingProvider for SpyEmbedder {
    fn provider_name(&self) -> &str {
        "spy-hash"
    }

    fn embed_batch(&self, texts: &[String]) -> Result<Option<Vec<Vec<f32>>>> {
        self.batch_calls.fetch_add(1, Ordering::Relaxed);
        self.texts_embedded
            .fetch_add(texts.len(), Ordering::Relaxed);
        self.inner.embed_batch(texts)
    }

    fn embed_query(&self, query: &str) -> Result<Option<Vec<f32>>> {
        self.inner.embed_query(query)
    }
}

#[test]
fn branch_isolation_and_rewind_cleanup_in_hybrid_search() {
    let store = SqliteStore::open_in_memory().unwrap();
    let vcs = MemVcs::new(store.clone());
    vcs.init().unwrap();

    vcs.write(
        "main",
        make_node(
            "/semantic/base.md",
            "# Base Architecture\nCore storage engine facts.",
            0.8,
            vec!["arch"],
        ),
    )
    .unwrap();
    vcs.write(
        "main",
        make_node(
            "/procedural/base-sop.md",
            "# Base SOP\nSee [[/semantic/base.md]] for architecture.",
            0.7,
            vec!["sop"],
        ),
    )
    .unwrap();
    let c1_main = vcs
        .commit("main", "main-agent", "commit base wiki")
        .unwrap();

    // Branch B writes a secret note linking back to /semantic/base.md, plus an uncommitted draft.
    vcs.create_branch("branch-b", &Revision::BranchHead("main".into()))
        .unwrap();
    vcs.write(
        "branch-b",
        make_node(
            "/semantic/secret_quantum.md",
            "# Secret Quantum\nZetaQuantumSecret99 references [[/semantic/base.md]].",
            0.9,
            vec!["secret"],
        ),
    )
    .unwrap();
    vcs.commit("branch-b", "subagent-b", "commit secret on branch-b")
        .unwrap();
    vcs.write(
        "branch-b",
        make_node(
            "/procedural/uncommitted_secret.md",
            "UncommittedBranchBToken only in branch-b workspace.",
            0.5,
            vec![],
        ),
    )
    .unwrap();

    let engine = MemSearchEngine::new(
        vcs.clone(),
        Arc::new(DeterministicHashEmbedder::default()),
        SearchConfig::default(),
    );

    // Searching on main must NEVER return branch-b committed or uncommitted secrets.
    let main_secret_hits = engine
        .search(&SearchRequest::new(
            Revision::BranchWorkspace("main".into()),
            "ZetaQuantumSecret99",
        ))
        .unwrap();
    assert!(
        main_secret_hits.is_empty(),
        "main leaked branch-b secret: {main_secret_hits:?}"
    );

    let main_uncommitted_hits = engine
        .search(&SearchRequest::new(
            Revision::BranchWorkspace("main".into()),
            "UncommittedBranchBToken",
        ))
        .unwrap();
    assert!(main_uncommitted_hits.is_empty());

    // Backlinks of /semantic/base.md on main must NOT expose /semantic/secret_quantum.md.
    let base_hits_main = engine
        .search(&SearchRequest::new(
            Revision::BranchWorkspace("main".into()),
            "Core storage engine",
        ))
        .unwrap();
    let base_hit = base_hits_main
        .iter()
        .find(|h| h.path == vp("/semantic/base.md"))
        .expect("expected /semantic/base.md on main");
    assert_eq!(base_hit.backlinks, vec![vp("/procedural/base-sop.md")]);

    // On branch-b, the secret IS searchable and its outlinks point to /semantic/base.md.
    let branch_b_hits = engine
        .search(&SearchRequest::new(
            Revision::BranchWorkspace("branch-b".into()),
            "ZetaQuantumSecret99",
        ))
        .unwrap();
    assert!(branch_b_hits
        .iter()
        .any(|h| h.path == vp("/semantic/secret_quantum.md")
            && h.outlinks.contains(&vp("/semantic/base.md"))));

    // Commit a hallucinated note on main, verify it is indexed, then rewind main back to c1_main.
    vcs.write(
        "main",
        make_node(
            "/semantic/hallucination.md",
            "BadHallucinationKeyword wrongly links [[/semantic/base.md]].",
            0.9,
            vec![],
        ),
    )
    .unwrap();
    vcs.commit("main", "agent", "hallucinated write").unwrap();

    let hallucination_hits = engine
        .search(&SearchRequest::new(
            Revision::BranchWorkspace("main".into()),
            "BadHallucinationKeyword",
        ))
        .unwrap();
    assert_eq!(hallucination_hits[0].path, vp("/semantic/hallucination.md"));
    assert!(hallucination_hits[0]
        .outlinks
        .contains(&vp("/semantic/base.md")));
    assert!(hallucination_hits
        .iter()
        .any(|h| h.path == vp("/semantic/base.md")
            && h.graph_score > 0.0
            && h.backlinks.contains(&vp("/semantic/hallucination.md"))));

    vcs.rewind("main", &c1_main).unwrap();
    let after_rewind = engine
        .search(&SearchRequest::new(
            Revision::BranchWorkspace("main".into()),
            "BadHallucinationKeyword",
        ))
        .unwrap();
    assert!(after_rewind.is_empty());

    let base_after_rewind = engine
        .search(&SearchRequest::new(
            Revision::BranchWorkspace("main".into()),
            "Core storage engine",
        ))
        .unwrap();
    let base_rewound = base_after_rewind
        .iter()
        .find(|h| h.path == vp("/semantic/base.md"))
        .unwrap();
    assert_eq!(base_rewound.backlinks, vec![vp("/procedural/base-sop.md")]);

    // Delete branch-b and run GC -> orphaned FTS5 rows & chunks must be cleaned too.
    vcs.delete_branch("branch-b").unwrap();
    let gc_stats = vcs.gc().unwrap();
    assert!(gc_stats.removed_blobs >= 2);

    let fts_remaining: i64 = store
        .with_conn(|conn| {
            conn.query_row("SELECT COUNT(*) FROM fts_chunks", [], |r| r.get(0))
                .map_err(|e| mem_core::MemError::Storage(e.to_string()))
        })
        .unwrap();
    assert_eq!(fts_remaining, 2);
}

#[test]
fn deduplicated_indexing_across_50_branches() {
    let store = SqliteStore::open_in_memory().unwrap();
    let vcs = MemVcs::new(store.clone());
    vcs.init().unwrap();

    for i in 0..3 {
        vcs.write(
            "main",
            make_node(
                &format!("/semantic/shared_{i}.md"),
                &format!("# Shared Note {i}\nShared deduplicated knowledge payload #{i}."),
                0.7,
                vec!["shared"],
            ),
        )
        .unwrap();
    }
    vcs.commit("main", "init", "add 3 shared notes").unwrap();

    for b in 0..50 {
        vcs.create_branch(
            &format!("exp/b{b:02}"),
            &Revision::BranchHead("main".into()),
        )
        .unwrap();
    }

    let spy = Arc::new(SpyEmbedder::new());
    let engine = MemSearchEngine::new(vcs, spy.clone(), SearchConfig::default());

    for b in 0..50 {
        let rev = Revision::BranchHead(format!("exp/b{b:02}"));
        let hits = engine
            .search(&SearchRequest::new(rev, "deduplicated knowledge"))
            .unwrap();
        assert_eq!(hits.len(), 3);
    }

    assert_eq!(
        spy.batch_calls.load(Ordering::Relaxed),
        1,
        "embed_batch must run only once across 50 branches sharing identical blobs"
    );
    assert_eq!(spy.texts_embedded.load(Ordering::Relaxed), 3);
    assert_eq!(engine.total_embedding_calls(), 1);

    let chunk_rows: i64 = store
        .with_conn(|conn| {
            conn.query_row("SELECT COUNT(*) FROM chunk_index", [], |r| r.get(0))
                .map_err(|e| mem_core::MemError::Storage(e.to_string()))
        })
        .unwrap();
    assert_eq!(chunk_rows, 3);
}

#[test]
fn obsidian_wikilinks_aliases_and_backlinks_boost_recall() {
    let store = SqliteStore::open_in_memory().unwrap();
    let vcs = MemVcs::new(store);
    vcs.init().unwrap();

    // Note A: declares alias BUSY_TIMEOUT and outlinks to Note B.
    let note_a = r#"---
title: SQLite 并发与锁配置
aliases: ["BUSY_TIMEOUT", "数据库写锁"]
tags: ["db/sqlite", "storage"]
entities:
  - "SQLite -> uses -> WAL Mode"
importance: 0.92
---
# 存储引擎
## 并发设置
开启 WAL 模式并配置超时，详细排查手册见 [[/procedural/sqlite-lock-sop.md|锁处理SOP]]。
"#;

    // Note B: does NOT mention "BUSY_TIMEOUT" anywhere! Only reachable via 1-hop Wikilink from Note A.
    let note_b = r#"---
title: 数据库连接锁定排查指南
tags: ["ops/runbook"]
importance: 0.85
---
# 排查步骤
1. 检查是否存在长时间未提交的事务。
2. 检查只读连接是否阻塞了 checkpoint。
"#;

    // Note C: does NOT mention "BUSY_TIMEOUT", but links TO Note A by title (creates a backlink to Note A).
    let note_c = r#"---
title: 架构决策索引
tags: ["moc"]
importance: 0.75
---
# 存储选型 MOC
核心并发参数规范见 [[SQLite 并发与锁配置]]。
"#;

    vcs.write(
        "main",
        make_node("/semantic/sqlite-wal.md", note_a, 0.92, vec!["db/sqlite"]),
    )
    .unwrap();
    vcs.write(
        "main",
        make_node(
            "/procedural/sqlite-lock-sop.md",
            note_b,
            0.85,
            vec!["ops/runbook"],
        ),
    )
    .unwrap();
    vcs.write(
        "main",
        make_node("/semantic/architecture-notes.md", note_c, 0.75, vec!["moc"]),
    )
    .unwrap();
    vcs.commit("main", "architect", "add obsidian wiki graph notes")
        .unwrap();

    // Verify Level-1 pure BM25 + Obsidian Wiki Graph (NoopEmbedder) recalls Note A, Note B (outlink), and Note C (backlink).
    let engine_l1 = MemSearchEngine::new_bm25_only(vcs.clone());
    let hits_l1 = engine_l1
        .search(&SearchRequest::new(
            Revision::BranchHead("main".into()),
            "BUSY_TIMEOUT",
        ))
        .unwrap();

    assert_eq!(hits_l1[0].path, vp("/semantic/sqlite-wal.md"));
    assert_eq!(
        hits_l1[0].outlinks,
        vec![vp("/procedural/sqlite-lock-sop.md")]
    );
    assert_eq!(
        hits_l1[0].backlinks,
        vec![vp("/semantic/architecture-notes.md")]
    );

    let sop_hit = hits_l1
        .iter()
        .find(|h| h.path == vp("/procedural/sqlite-lock-sop.md"))
        .expect("Note B must be recalled via 1-hop Wikilink outlink expansion");
    assert!(sop_hit.graph_score > 0.0);
    assert_eq!(sop_hit.backlinks, vec![vp("/semantic/sqlite-wal.md")]);

    let moc_hit = hits_l1
        .iter()
        .find(|h| h.path == vp("/semantic/architecture-notes.md"))
        .expect("Note C must be recalled via 1-hop Wikilink backlink expansion");
    assert!(moc_hit.graph_score > 0.0);
    assert_eq!(moc_hit.outlinks, vec![vp("/semantic/sqlite-wal.md")]);

    // Verify Chinese alias and partial CJK phrase queries also recall Note A + 1-hop linked Note B.
    let cjk_hits = engine_l1
        .search(&SearchRequest::new(
            Revision::BranchHead("main".into()),
            "数据库写锁",
        ))
        .unwrap();
    assert_eq!(cjk_hits[0].path, vp("/semantic/sqlite-wal.md"));
    assert!(cjk_hits
        .iter()
        .any(|h| h.path == vp("/procedural/sqlite-lock-sop.md")));

    let partial_cjk_hits = engine_l1
        .search(&SearchRequest::new(
            Revision::BranchHead("main".into()),
            "并发锁",
        ))
        .unwrap();
    assert_eq!(partial_cjk_hits[0].path, vp("/semantic/sqlite-wal.md"));

    // Verify Level-2 hybrid mode with DeterministicHashEmbedder produces same high-recall graph behavior.
    let engine_l2 = MemSearchEngine::new(
        vcs,
        Arc::new(DeterministicHashEmbedder::default()),
        SearchConfig::default(),
    );
    let hits_l2 = engine_l2
        .search(&SearchRequest::new(
            Revision::BranchHead("main".into()),
            "BUSY_TIMEOUT",
        ))
        .unwrap();
    assert_eq!(hits_l2[0].path, vp("/semantic/sqlite-wal.md"));
    assert!(hits_l2
        .iter()
        .any(|h| h.path == vp("/procedural/sqlite-lock-sop.md")));
}

#[test]
fn ollama_and_openai_provider_config_switching_and_http_client() {
    // 1. Preset serialization & dev -> prod config switching.
    let dev_cfg = MemConfig::ollama_dev();
    assert_eq!(dev_cfg.embedding.backend, ProviderBackend::Ollama);
    assert_eq!(dev_cfg.embedding.base_url, "http://127.0.0.1:11434/v1");
    assert_eq!(dev_cfg.embedding.model, "bge-m3");
    assert_eq!(dev_cfg.consolidate_llm.model, "qwen2.5:7b");

    let prod_cfg = MemConfig::openai_prod("sk-prod-key");
    assert_eq!(prod_cfg.embedding.backend, ProviderBackend::OpenAi);
    assert_eq!(prod_cfg.embedding.base_url, "https://api.openai.com/v1");
    assert_eq!(prod_cfg.embedding.model, "text-embedding-3-small");
    assert_eq!(prod_cfg.embedding.api_key.as_deref(), Some("sk-prod-key"));
    assert_eq!(prod_cfg.consolidate_llm.model, "gpt-6-luna");

    let serialized = serde_json::to_string(&prod_cfg).unwrap();
    let roundtrip: MemConfig = serde_json::from_str(&serialized).unwrap();
    assert_eq!(roundtrip, prod_cfg);

    // 2. Env lookup switching: Ollama -> OpenAI -> Disabled.
    let mut env_map = HashMap::new();
    env_map.insert("MEM_PROVIDER", "openai");
    env_map.insert("OPENAI_API_KEY", "sk-env-secret");
    env_map.insert("MEM_EMBEDDING_DIMENSIONS", "512");
    let emb_env =
        EmbeddingProviderConfig::from_env_lookup(|k| env_map.get(k).map(|v| (*v).to_string()));
    assert_eq!(emb_env.backend, ProviderBackend::OpenAi);
    assert_eq!(emb_env.model, "text-embedding-3-small");
    assert_eq!(emb_env.api_key.as_deref(), Some("sk-env-secret"));
    assert_eq!(emb_env.dimensions, Some(512));

    let llm_env =
        ConsolidateLlmConfig::from_env_lookup(|k| env_map.get(k).map(|v| (*v).to_string()));
    assert_eq!(llm_env.backend, ProviderBackend::OpenAi);
    assert_eq!(llm_env.model, "gpt-6-luna");

    // 3. Real local TCP loopback HTTP server testing OpenAiCompatibleEmbedder end-to-end.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let captured_auth: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let captured_path: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let auth_clone = captured_auth.clone();
    let path_clone = captured_path.clone();

    let server_handle = std::thread::spawn(move || {
        // Expect 2 requests: 1 batch embed during indexing + 1 query embed during search.
        for _ in 0..2 {
            let (mut stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut request_line = String::new();
            reader.read_line(&mut request_line).unwrap();
            let req_path = request_line
                .split_whitespace()
                .nth(1)
                .unwrap_or("")
                .to_string();
            *path_clone.lock().unwrap() = Some(req_path);

            let mut content_len = 0usize;
            loop {
                let mut header = String::new();
                reader.read_line(&mut header).unwrap();
                let trimmed = header.trim_end_matches(['\r', '\n']);
                if trimmed.is_empty() {
                    break;
                }
                if let Some((k, v)) = trimmed.split_once(':') {
                    if k.eq_ignore_ascii_case("authorization") {
                        *auth_clone.lock().unwrap() = Some(v.trim().to_string());
                    } else if k.eq_ignore_ascii_case("content-length") {
                        content_len = v.trim().parse().unwrap_or(0);
                    }
                }
            }

            let mut body_bytes = vec![0u8; content_len];
            reader.read_exact(&mut body_bytes).unwrap();
            let req_json: serde_json::Value = serde_json::from_slice(&body_bytes).unwrap();
            let inputs = req_json["input"].as_array().unwrap();

            let data: Vec<serde_json::Value> = inputs
                .iter()
                .enumerate()
                .map(|(idx, val)| {
                    let text = val.as_str().unwrap_or("");
                    let vec = if text.contains("alpha") {
                        vec![1.0, 0.0]
                    } else {
                        vec![0.0, 1.0]
                    };
                    serde_json::json!({ "index": idx, "embedding": vec })
                })
                .collect();

            let resp_body = serde_json::json!({ "data": data }).to_string();
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                resp_body.len(),
                resp_body
            );
            stream.write_all(resp.as_bytes()).unwrap();
        }
    });

    let cfg = EmbeddingProviderConfig {
        backend: ProviderBackend::Ollama,
        base_url: format!("http://127.0.0.1:{port}/v1"),
        model: "bge-m3".to_string(),
        api_key: Some("sk-local-test".to_string()),
        dimensions: Some(2),
    };
    let embedder = Arc::new(OpenAiCompatibleEmbedder::new(cfg));

    let store = SqliteStore::open_in_memory().unwrap();
    let vcs = MemVcs::new(store);
    vcs.init().unwrap();
    vcs.write(
        "main",
        make_node("/semantic/doc_alpha.md", "alpha semantics", 0.9, vec![]),
    )
    .unwrap();
    vcs.write(
        "main",
        make_node("/semantic/doc_beta.md", "beta semantics", 0.9, vec![]),
    )
    .unwrap();
    vcs.commit("main", "tester", "add docs").unwrap();

    let engine = MemSearchEngine::new(vcs, embedder, SearchConfig::default());
    let hits = engine
        .search(&SearchRequest::new(
            Revision::BranchHead("main".into()),
            "alpha",
        ))
        .unwrap();

    server_handle.join().unwrap();

    assert_eq!(
        captured_path.lock().unwrap().as_deref(),
        Some("/v1/embeddings")
    );
    assert_eq!(
        captured_auth.lock().unwrap().as_deref(),
        Some("Bearer sk-local-test")
    );
    assert_eq!(hits[0].path, vp("/semantic/doc_alpha.md"));
    assert_eq!(hits[0].vector_rank, Some(1));

    // 4. Verify production OpenAI config via injected HTTPS transport + Disabled fallback.
    let prod_embedder = OpenAiCompatibleEmbedder::with_transport(
        EmbeddingProviderConfig::openai_production("sk-prod-key"),
        Arc::new(|url, auth, body| {
            assert_eq!(url, "https://api.openai.com/v1/embeddings");
            assert_eq!(auth, Some("sk-prod-key"));
            let parsed: serde_json::Value = serde_json::from_str(body).unwrap();
            assert_eq!(parsed["model"], "text-embedding-3-small");
            assert_eq!(parsed["dimensions"], 1536);
            Ok(serde_json::json!({
                "data": [{ "index": 0, "embedding": [0.25, 0.75] }]
            })
            .to_string())
        }),
    );
    let prod_vec = prod_embedder.embed_query("prod test").unwrap().unwrap();
    assert_eq!(prod_vec, vec![0.25, 0.75]);

    let disabled_embedder = OpenAiCompatibleEmbedder::new(EmbeddingProviderConfig::disabled());
    assert!(disabled_embedder.embed_query("disabled").unwrap().is_none());
}

#[test]
fn core_compiler_budget_and_local_obsidian_vault_import() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("mem_vault.db");
    let store = SqliteStore::open(&db_path).unwrap();
    let vcs = MemVcs::new(store.clone());
    let c0 = vcs.init().unwrap();

    // 1. Test /core hot compiler token budget & priority sorting + <5ms latency at 10k tree scale.
    vcs.write(
        "main",
        make_node(
            "/core/rules.md",
            "Never mutate production DB directly.",
            1.0,
            vec!["rule"],
        ),
    )
    .unwrap();
    vcs.write(
        "main",
        make_node(
            "/core/goal.md",
            "Ship Obsidian Wiki graph search.",
            0.9,
            vec!["goal"],
        ),
    )
    .unwrap();
    let long_optional = "extra ".repeat(400);
    vcs.write(
        "main",
        make_node("/core/optional_notes.md", &long_optional, 0.2, vec![]),
    )
    .unwrap();

    // Populate 10,000 semantic nodes in a transaction to verify compile_core stays fast at scale.
    store
        .with_tx(|tx| {
            let meta_json = MemoryMetadata::default().canonical_json()?;
            for i in 0..10_000usize {
                let bucket = i / 100;
                let path = VirtualPath::new(format!("/semantic/b_{bucket:03}/n_{i:05}.md"))?;
                let payload = format!("semantic note #{i}");
                let blob_id =
                    mem_store::cas::put_blob_conn(tx, payload.as_bytes(), "text/markdown")?;
                mem_store::workspace_upsert_conn(tx, "main", &path, &blob_id, &meta_json)?;
            }
            Ok(())
        })
        .unwrap();
    vcs.commit("main", "bench", "commit 10,003 nodes").unwrap();

    let start = Instant::now();
    let compiled = compile_core(&vcs, &Revision::BranchHead("main".into()), 120).unwrap();
    let elapsed = start.elapsed();

    assert!(
        elapsed < Duration::from_millis(5),
        "expected compile_core < 5ms under 10k nodes, took {elapsed:?}"
    );
    assert_eq!(
        compiled.included_paths,
        vec![vp("/core/rules.md"), vp("/core/goal.md")]
    );
    assert_eq!(compiled.skipped_paths, vec![vp("/core/optional_notes.md")]);
    assert!(compiled
        .rendered
        .contains("Never mutate production DB directly."));
    assert!(compiled.estimated_tokens <= 120);

    // 2. Test local Obsidian Vault directory import (skipping .obsidian/).
    let vault_dir = dir.path().join("my_obsidian_vault");
    fs::create_dir_all(vault_dir.join(".obsidian")).unwrap();
    fs::create_dir_all(vault_dir.join("notes")).unwrap();
    fs::create_dir_all(vault_dir.join("runbooks")).unwrap();

    fs::write(vault_dir.join(".obsidian/app.json"), r#"{"ignored": true}"#).unwrap();
    fs::write(
        vault_dir.join("notes/adr-001.md"),
        "---\ntitle: ADR 001 Storage\naliases: [\"CAS Storage\"]\ntags: [adr]\nimportance: 0.95\n---\n# ADR 001\nUse BLAKE3 CAS and check [[fix-lock]].\n",
    )
    .unwrap();
    fs::write(
        vault_dir.join("runbooks/fix-lock.md"),
        "---\ntitle: Lock Recovery Runbook\nimportance: 0.85\n---\n# Fix Lock\nEnsure busy_timeout is configured.\n",
    )
    .unwrap();

    vcs.create_branch("vault_import", &Revision::Commit(c0))
        .unwrap();
    let engine = MemSearchEngine::new_bm25_only(vcs);
    let import_stats = engine
        .import_vault(&vault_dir, "vault_import", MemoryTier::Semantic)
        .unwrap();

    assert_eq!(import_stats.files_imported, 2);
    assert_eq!(import_stats.blobs_newly_indexed, 2);
    assert!(import_stats.links_extracted >= 2);

    let vault_hits = engine
        .search(
            &SearchRequest::new(
                Revision::BranchWorkspace("vault_import".into()),
                "CAS Storage",
            )
            .with_prefix(vp("/semantic/notes"))
            .with_top_k(5),
        )
        .unwrap();
    assert_eq!(vault_hits.len(), 1);
    assert_eq!(vault_hits[0].path, vp("/semantic/notes/adr-001.md"));

    // Without prefix restriction under /semantic/notes, 1-hop Wikilink recalls runbooks/fix-lock.md too.
    let cross_hits = engine
        .search(
            &SearchRequest::new(
                Revision::BranchWorkspace("vault_import".into()),
                "CAS Storage",
            )
            .with_prefix(vp("/semantic")),
        )
        .unwrap();
    assert!(cross_hits
        .iter()
        .any(|h| h.path == vp("/semantic/runbooks/fix-lock.md")));
}

#[test]
fn collaborative_pm_dev_bot_ingest_draft_merge_and_vault_roundtrip() {
    let dir = tempdir().unwrap();
    let vault = dir.path().join("collab_vault");
    let tpl_stats = init_vault_template(&vault).unwrap();
    assert_eq!(tpl_stats.files_created, 6);

    // Developer adds architecture note inside multi-tier Obsidian vault.
    fs::write(
        vault.join("semantic/architecture/wal-mode.md"),
        "---\ntitle: \"SQLite WAL Mode\"\naliases: [\"WAL\"]\ntags: [\"role/dev\", \"sqlite\"]\nimportance: 0.90\n---\n\n# SQLite WAL Mode\n\nUse WAL journaling and busy_timeout=5000ms.\n",
    )
    .unwrap();

    let store = SqliteStore::open_in_memory().unwrap();
    let vcs = MemVcs::new(store);
    vcs.init().unwrap();
    let engine = MemSearchEngine::new_bm25_only(vcs.clone());

    let sync_stats = engine.sync_vault(&vault, "main").unwrap();
    assert_eq!(sync_stats.files_imported, 2); // core/system.md + semantic/architecture/wal-mode.md
    vcs.commit("main", "dev@engram", "sync starter vault & wal ADR")
        .unwrap();

    // PM ingests a natural-language note into --draft (isolated in draft/pm-inbox).
    let pm_req = IngestRequest::new(
        IngestRole::Pm,
        "Checkout retry policy must tolerate WAL lock contention up to 3 attempts.",
    )
    .with_title("Checkout Retry Policy")
    .with_tags(["checkout", "prd"])
    .with_draft(true);

    let pm_out = engine.ingest(&pm_req, None).unwrap();
    assert_eq!(pm_out.branch, "draft/pm-inbox");
    assert_eq!(
        pm_out.path,
        vp("/semantic/product/checkout-retry-policy.md")
    );
    assert!(pm_out.markdown.contains("[[SQLite WAL Mode"));
    assert!(vcs
        .read(&Revision::BranchWorkspace("main".into()), &pm_out.path)
        .unwrap()
        .is_none());

    // Merge approved PM draft into main and verify cross-role graph recall.
    vcs.merge("draft/pm-inbox", "main", MergeStrategy::Auto)
        .unwrap();
    let main_hits = engine
        .search(&SearchRequest::new(
            Revision::BranchWorkspace("main".into()),
            "SQLite WAL Mode",
        ))
        .unwrap();
    assert!(main_hits
        .iter()
        .any(|h| h.path == vp("/semantic/architecture/wal-mode.md")));
    assert!(main_hits
        .iter()
        .any(|h| h.path == vp("/semantic/product/checkout-retry-policy.md")));

    // Bot records an episodic observation on main.
    let bot_req = IngestRequest::new(
        IngestRole::Bot,
        "Observed checkout retries succeeding on attempt 2 during load test.",
    )
    .with_title("Checkout Retry Load Observation");
    let bot_out = engine.ingest(&bot_req, None).unwrap();
    assert!(bot_out.path.as_str().starts_with("/episodic/bot-insights/"));

    // Export main snapshot back to local Obsidian directory layout.
    let export_dir = dir.path().join("exported_vault");
    let export_stats = engine
        .export_vault(&Revision::BranchWorkspace("main".into()), &export_dir)
        .unwrap();
    assert_eq!(export_stats.files_exported, 4);
    assert!(export_dir.join("core/system.md").exists());
    assert!(export_dir
        .join("semantic/architecture/wal-mode.md")
        .exists());
    assert!(export_dir
        .join("semantic/product/checkout-retry-policy.md")
        .exists());
    assert!(export_dir
        .join(bot_out.path.as_str().trim_start_matches('/'))
        .exists());
}
