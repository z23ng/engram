use std::sync::{Arc, Mutex};

use mem_consolidate::{ConsolidationEngine, ConsolidationMergeStatus, ConsolidationRequest};
use mem_core::{MemoryMetadata, MemoryNode, MemoryTier, MergeStrategy, Revision, VirtualPath};
use mem_search::{ConsolidateLlmConfig, MemSearchEngine, ProviderBackend};
use mem_store::SqliteStore;
use mem_vcs::{MemVcs, VersionControlEngine};
use serde_json::json;

fn vp(s: &str) -> VirtualPath {
    VirtualPath::new(s).expect("valid virtual path")
}

fn make_episode(path: &str, body: &str, importance: f32, tags: Vec<&str>, ts: i64) -> MemoryNode {
    MemoryNode::new(vp(path), body.as_bytes().to_vec()).with_metadata(MemoryMetadata {
        content_type: "text/markdown".to_string(),
        importance,
        tags: tags.into_iter().map(String::from).collect(),
        created_at_ms: ts,
        updated_at_ms: ts,
        ttl_seconds: None,
    })
}

fn setup_engine() -> MemSearchEngine {
    let store = SqliteStore::open_in_memory().unwrap();
    let vcs = MemVcs::new(store);
    vcs.init().unwrap();
    MemSearchEngine::new_bm25_only(vcs)
}

#[test]
fn consolidation_runs_in_isolated_branch_and_weaves_wikilinks_without_polluting_main() {
    let search = setup_engine();
    let vcs = search.vcs();

    // Seed an active architecture card on main so consolidation weaves [[Wikilinks]].
    let arch_path = vp("/semantic/architecture/sqlite-wal.md");
    let arch_blob = vcs
        .write_node(
            "main",
            MemoryNode::new(
                arch_path.clone(),
                b"---\ntitle: \"SQLite WAL Mode\"\naliases: [\"WAL Mode\", \"BUSY_TIMEOUT\"]\n---\n\n# SQLite WAL Mode\n\nReaders never block writers.\n".to_vec(),
            ),
        )
        .unwrap();
    search.index_blob_if_needed(&arch_path, &arch_blob).unwrap();

    // Seed overlapping episodic logs (semantic + procedural).
    let ep1 = make_episode(
        "/episodic/session-01.md",
        "# Session 01\n- Confirmed SQLite WAL Mode prevents write lock contention under concurrent readers.\n- SOP: Always configure PRAGMA busy_timeout = 5000 before starting workers.\n",
        0.7,
        vec!["episodic", "sqlite"],
        1_700_000_001_000,
    );
    let ep2 = make_episode(
        "/episodic/session-02.md",
        "# Session 02\n- Confirmed SQLite WAL Mode prevents write lock contention under concurrent readers.\n- Runbook: Check WAL checkpoint status if read latency spikes.\n",
        0.8,
        vec!["episodic", "sqlite"],
        1_700_000_002_000,
    );
    vcs.write("main", ep1).unwrap();
    vcs.write("main", ep2).unwrap();
    vcs.commit("main", "bot", "seed episodes").unwrap();

    let consolidator = ConsolidationEngine::new(search.clone());
    let req = ConsolidationRequest::new("main");
    let report = consolidator.consolidate(&req).unwrap();

    assert!(!report.dry_run);
    assert!(!report.used_llm);
    assert_eq!(report.episodes_scanned, 2);
    assert!(matches!(
        report.merge_status,
        ConsolidationMergeStatus::Skipped
    ));

    let cons_branch = report.consolidation_branch.as_ref().unwrap();
    assert!(cons_branch.starts_with("consolidate/"));
    assert!(report.commit_id.is_some());
    assert!(!report.distilled_cards.is_empty());

    // Should produce both Semantic and Procedural cards and deduplicate the repeated WAL fact.
    let semantic_cards: Vec<_> = report
        .distilled_cards
        .iter()
        .filter(|c| c.tier == MemoryTier::Semantic)
        .collect();
    let procedural_cards: Vec<_> = report
        .distilled_cards
        .iter()
        .filter(|c| c.tier == MemoryTier::Procedural)
        .collect();
    assert!(!semantic_cards.is_empty());
    assert!(!procedural_cards.is_empty());

    let sem_md = &semantic_cards[0].markdown;
    assert_eq!(
        sem_md
            .matches("prevents write lock contention under concurrent readers")
            .count(),
        1
    );
    assert!(sem_md.contains("[[SQLite WAL Mode]]"));

    // Main must remain completely untouched before merge!
    for card in &report.distilled_cards {
        assert!(vcs
            .read(&Revision::BranchWorkspace("main".into()), &card.path)
            .unwrap()
            .is_none());
        assert!(vcs
            .read(&Revision::BranchHead(cons_branch.clone()), &card.path)
            .unwrap()
            .is_some());
    }

    // Deleting the consolidation branch discards everything cleanly.
    vcs.delete_branch(cons_branch).unwrap();
    assert!(vcs.head_commit(cons_branch).is_err());
}

#[test]
fn consolidation_auto_merge_and_archive_decays_importance_and_skips_reprocessing() {
    let search = setup_engine();
    let vcs = search.vcs();

    vcs.write(
        "main",
        make_episode(
            "/episodic/day1.md",
            "- Payment gateway retries must attach X-Idempotency-Key header.\n",
            0.9,
            vec!["episodic", "payments"],
            1_700_000_010_000,
        ),
    )
    .unwrap();
    vcs.commit("main", "bot", "day 1").unwrap();

    let consolidator = ConsolidationEngine::new(search.clone());
    let req = ConsolidationRequest::new("main")
        .with_auto_merge(true)
        .with_merge_strategy(MergeStrategy::Auto)
        .with_delete_branch_on_merge(true)
        .with_archive_processed(true)
        .with_decay_factor(0.5);

    let report = consolidator.consolidate(&req).unwrap();
    assert!(matches!(
        report.merge_status,
        ConsolidationMergeStatus::FastForward { .. } | ConsolidationMergeStatus::Merged { .. }
    ));
    assert!(report.consolidation_branch_deleted);
    assert!(report.archive_path.is_some());

    // Distilled card and archive digest are now present on main.
    let card_path = &report.distilled_cards[0].path;
    assert!(vcs
        .read(&Revision::BranchHead("main".into()), card_path)
        .unwrap()
        .is_some());
    assert!(vcs
        .read(
            &Revision::BranchHead("main".into()),
            report.archive_path.as_ref().unwrap()
        )
        .unwrap()
        .is_some());

    // Source episode importance decayed and tagged archived-episode.
    let updated_ep = vcs
        .read(&Revision::BranchHead("main".into()), &vp("/episodic/day1.md"))
        .unwrap()
        .unwrap();
    assert!((updated_ep.metadata.importance - 0.45).abs() < 0.01);
    assert!(updated_ep
        .metadata
        .tags
        .contains(&"archived-episode".to_string()));

    // Running consolidation again should find 0 unarchived episodes.
    let second = consolidator.consolidate(&req).unwrap();
    assert_eq!(second.episodes_scanned, 0);
}

#[test]
fn consolidation_dry_run_creates_no_branches_or_commits() {
    let search = setup_engine();
    let vcs = search.vcs().clone();

    vcs.write(
        "main",
        make_episode(
            "/episodic/dry.md",
            "- Always verify BLAKE3 digests on blob decompression.\n",
            0.8,
            vec!["episodic", "storage"],
            1_700_000_020_000,
        ),
    )
    .unwrap();
    vcs.commit("main", "bot", "seed dry").unwrap();

    let refs_before = vcs.store().list_refs().unwrap();
    let commits_before = vcs.store().commit_count().unwrap();

    let consolidator = ConsolidationEngine::new(search);
    let report = consolidator
        .consolidate(&ConsolidationRequest::new("main").with_dry_run(true))
        .unwrap();

    assert!(report.dry_run);
    assert!(report.consolidation_branch.is_none());
    assert!(report.commit_id.is_none());
    assert_eq!(report.distilled_cards.len(), 1);
    assert_eq!(vcs.store().list_refs().unwrap(), refs_before);
    assert_eq!(vcs.store().commit_count().unwrap(), commits_before);
}

#[test]
fn consolidation_supports_mock_llm_and_graceful_or_strict_fallback() {
    let search = setup_engine();
    let vcs = search.vcs();

    vcs.write(
        "main",
        make_episode(
            "/episodic/llm-src.md",
            "- Observed 429 rate limits when batching >100 embedding chunks at once.\n",
            0.85,
            vec!["episodic", "embeddings"],
            1_700_000_030_000,
        ),
    )
    .unwrap();
    vcs.commit("main", "bot", "seed llm episode").unwrap();

    let captured_url = Arc::new(Mutex::new(String::new()));
    let captured_clone = Arc::clone(&captured_url);

    let mock_transport: mem_consolidate::LlmChatTransport = Arc::new(move |url, _key, body| {
        *captured_clone.lock().unwrap() = url.to_string();
        assert!(body.contains("gpt-6-luna"));
        Ok(json!({
            "id": "cmpl-1",
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": "```json\n{\"semantic_cards\":[{\"title\":\"Embedding Batch Sizing\",\"slug\":\"embedding-batch-sizing\",\"aliases\":[\"429 rate limit\"],\"tags\":[\"embeddings\"],\"entities\":[\"Embedder -> caps_batch_at -> 100\"],\"importance\":0.88,\"facts\":[\"Embedding endpoints throttle requests above 100 chunks per batch.\"]}],\"procedural_cards\":[{\"title\":\"Handle Embedding 429\",\"slug\":\"handle-embedding-429\",\"aliases\":[],\"tags\":[\"ops\"],\"entities\":[],\"importance\":0.9,\"steps\":[\"Split embed_batch payloads to <= 64 chunks.\"]}]}\n```"
                }
            }]
        })
        .to_string())
    });

    let llm_cfg = ConsolidateLlmConfig {
        backend: ProviderBackend::OpenAi,
        base_url: "http://127.0.0.1:9999/v1".to_string(),
        model: "gpt-6-luna".to_string(),
        api_key: Some("sk-test".to_string()),
    };

    let consolidator = ConsolidationEngine::new(search.clone())
        .with_llm_config(llm_cfg.clone())
        .with_llm_transport(mock_transport);

    let report = consolidator
        .consolidate(&ConsolidationRequest::new("main").with_use_llm(true))
        .unwrap();

    assert!(report.used_llm);
    assert!(report.llm_fallback_reason.is_none());
    assert_eq!(
        captured_url.lock().unwrap().as_str(),
        "http://127.0.0.1:9999/v1/chat/completions"
    );
    assert_eq!(report.distilled_cards.len(), 2);

    // Failing LLM with strict_llm = false falls back deterministically.
    let failing_transport: mem_consolidate::LlmChatTransport =
        Arc::new(|_, _, _| Err(mem_core::MemError::Storage("simulated 503".to_string())));
    let fallback_engine = ConsolidationEngine::new(search.clone())
        .with_llm_config(llm_cfg)
        .with_llm_transport(failing_transport);

    let fallback_report = fallback_engine
        .consolidate(&ConsolidationRequest::new("main").with_use_llm(true))
        .unwrap();
    assert!(!fallback_report.used_llm);
    assert!(fallback_report
        .llm_fallback_reason
        .as_deref()
        .unwrap()
        .contains("simulated 503"));
    assert!(!fallback_report.distilled_cards.is_empty());

    // Failing LLM with strict_llm = true returns an error.
    let strict_err = fallback_engine.consolidate(
        &ConsolidationRequest::new("main")
            .with_use_llm(true)
            .with_strict_llm(true),
    );
    assert!(strict_err.is_err());
}
