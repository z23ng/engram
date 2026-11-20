//! Index freshness across embedding model changes, and the full rebuild.

use std::sync::Arc;

use mem_core::{MemoryNode, Revision, VirtualPath};
use mem_search::{DeterministicHashEmbedder, MemSearchEngine, SearchConfig, SearchRequest};
use mem_store::SqliteStore;
use mem_vcs::{MemVcs, VersionControlEngine};

fn repo() -> (tempfile::TempDir, MemVcs) {
    let dir = tempfile::tempdir().unwrap();
    let vcs = MemVcs::new(SqliteStore::open(dir.path().join("m.db")).unwrap());
    vcs.init().unwrap();
    (dir, vcs)
}

/// `None` = BM25 only (no embedder); `Some(dim)` = a model whose id includes `dim`.
fn engine(vcs: &MemVcs, dim: Option<usize>) -> MemSearchEngine {
    match dim {
        None => MemSearchEngine::new_bm25_only(vcs.clone()),
        Some(d) => MemSearchEngine::new(
            vcs.clone(),
            Arc::new(DeterministicHashEmbedder::new(d)),
            SearchConfig::default(),
        ),
    }
}

fn write(vcs: &MemVcs, branch: &str, path: &str, body: &str) {
    vcs.write_node(
        branch,
        MemoryNode::new(VirtualPath::new(path).unwrap(), body.as_bytes().to_vec()),
    )
    .unwrap();
}

/// (chunks, chunks with a vector, largest vector in bytes)
fn index_state(vcs: &MemVcs) -> (i64, i64, Option<i64>) {
    vcs.store()
        .with_conn(|c| {
            Ok(c.query_row(
                "SELECT count(*), count(embedding), max(length(embedding)) FROM chunk_index",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap())
        })
        .unwrap()
}

fn main_head() -> Revision {
    Revision::BranchHead("main".into())
}

#[test]
fn enabling_a_model_embeds_files_indexed_without_one_and_an_outage_keeps_vectors() {
    let (_d, vcs) = repo();
    write(
        &vcs,
        "main",
        "/semantic/a.md",
        "# A\n\nchannel routing rules",
    );
    vcs.commit("main", "t", "a").unwrap();

    engine(&vcs, None)
        .ensure_revision_indexed(&main_head(), None)
        .unwrap();
    let (chunks, with_vec, _) = index_state(&vcs);
    assert!(chunks > 0);
    assert_eq!(with_vec, 0, "BM25-only indexing stores no vectors");

    engine(&vcs, Some(64))
        .ensure_revision_indexed(&main_head(), None)
        .unwrap();
    assert_eq!(
        index_state(&vcs),
        (chunks, chunks, Some(64 * 4)),
        "model now active: every chunk gets a vector"
    );

    engine(&vcs, None)
        .ensure_revision_indexed(&main_head(), None)
        .unwrap();
    assert_eq!(
        index_state(&vcs),
        (chunks, chunks, Some(64 * 4)),
        "embedder outage must not drop vectors"
    );
}

#[test]
fn a_model_change_reembeds_on_the_next_search() {
    let (_d, vcs) = repo();
    write(
        &vcs,
        "main",
        "/semantic/a.md",
        "# A\n\nchannel routing rules",
    );
    engine(&vcs, Some(64))
        .ensure_revision_indexed(&Revision::BranchWorkspace("main".into()), None)
        .unwrap();
    assert_eq!(index_state(&vcs).2, Some(64 * 4));

    let req = SearchRequest::new(Revision::BranchWorkspace("main".into()), "routing");
    let hits = engine(&vcs, Some(32)).search(&req).unwrap();
    assert_eq!(hits[0].path.as_str(), "/semantic/a.md");
    let (chunks, with_vec, bytes) = index_state(&vcs);
    assert_eq!(
        (with_vec, bytes),
        (chunks, Some(32 * 4)),
        "old 64-dim vectors replaced, no duplicates"
    );
}

#[test]
fn reindex_all_rebuilds_every_branch_view() {
    let (_d, vcs) = repo();
    write(&vcs, "main", "/semantic/a.md", "# A\n\nfirst version");
    vcs.commit("main", "t", "a1").unwrap();
    vcs.create_branch("draft", &main_head()).unwrap();
    write(
        &vcs,
        "draft",
        "/semantic/b.md",
        "# B\n\nstaged on draft only",
    );
    write(&vcs, "main", "/semantic/a.md", "# A\n\nsecond version");
    vcs.commit("main", "t", "a2").unwrap();
    engine(&vcs, None)
        .ensure_revision_indexed(&main_head(), None)
        .unwrap();

    let stats = engine(&vcs, Some(64)).reindex_all().unwrap();
    assert_eq!(
        stats.blobs_newly_indexed, 3,
        "a2 on main, a1 and staged b on draft"
    );
    let (chunks, with_vec, _) = index_state(&vcs);
    assert_eq!(with_vec, chunks);
    let fts_rows: i64 = vcs
        .store()
        .with_conn(|c| {
            Ok(
                c.query_row("SELECT count(*) FROM fts_chunks", [], |r| r.get(0))
                    .unwrap(),
            )
        })
        .unwrap();
    assert_eq!(
        fts_rows, chunks,
        "full-text rows are rebuilt, not duplicated"
    );

    let req = SearchRequest::new(Revision::BranchWorkspace("draft".into()), "staged");
    let hits = engine(&vcs, None).search(&req).unwrap();
    assert_eq!(hits[0].path.as_str(), "/semantic/b.md");
}

/// Records the size of every `embed_batch` call; vectors from a 16-dim hash model.
struct Counting {
    inner: DeterministicHashEmbedder,
    calls: std::sync::Mutex<Vec<usize>>,
}

impl mem_search::EmbeddingProvider for Counting {
    fn provider_name(&self) -> &str {
        "counting"
    }
    fn model_id(&self) -> String {
        "counting:16".into()
    }
    fn embed_batch(&self, texts: &[String]) -> mem_core::Result<Option<Vec<Vec<f32>>>> {
        self.calls.lock().unwrap().push(texts.len());
        self.inner.embed_batch(texts)
    }
}

fn counting() -> Arc<Counting> {
    Arc::new(Counting {
        inner: DeterministicHashEmbedder::new(16),
        calls: Default::default(),
    })
}

fn many_files(vcs: &MemVcs, n: usize) {
    for i in 0..n {
        write(
            vcs,
            "main",
            &format!("/semantic/n{i}.md"),
            &format!("# Note {i}\n\nrouting fact {i}"),
        );
    }
    vcs.commit("main", "t", "many").unwrap();
}

#[test]
fn embedding_requests_are_split_into_groups_of_at_most_128_chunks() {
    let (_d, vcs) = repo();
    many_files(&vcs, 300);
    let emb = counting();
    let engine = MemSearchEngine::new(vcs.clone(), emb.clone(), SearchConfig::default());
    engine.reindex_all().unwrap();
    let calls = emb.calls.lock().unwrap().clone();
    assert!(
        calls.len() >= 3 && calls.iter().all(|&n| n <= 128),
        "{calls:?}"
    );
    let (chunks, with_vec, _) = index_state(&vcs);
    assert_eq!(with_vec, chunks);
}

#[test]
fn a_search_re_embeds_at_most_32_stale_files_including_unrecorded_vectors() {
    let (_d, vcs) = repo();
    many_files(&vcs, 50);
    engine(&vcs, None)
        .ensure_revision_indexed(&main_head(), None)
        .unwrap();
    let emb = counting();
    let hybrid = MemSearchEngine::new(vcs.clone(), emb.clone(), SearchConfig::default());
    hybrid
        .search(&SearchRequest::new(main_head(), "routing"))
        .unwrap();
    let (_, with_vec, _) = index_state(&vcs);
    assert_eq!(with_vec, 32, "one search embeds at most 32 stale files");
    hybrid
        .search(&SearchRequest::new(main_head(), "routing"))
        .unwrap();
    assert_eq!(index_state(&vcs).1, 50, "the next search catches up");

    // Vectors without a model record (databases older than blob_index_model) can come from any
    // model, so they are stale: re-embedded 32 per search, never adopted.
    vcs.store()
        .with_conn(|c| Ok(c.execute("DELETE FROM blob_index_model", []).unwrap()))
        .unwrap();
    hybrid
        .search(&SearchRequest::new(main_head(), "routing"))
        .unwrap();
    assert_eq!(model_rows(&vcs), 32);
}

fn model_rows(vcs: &MemVcs) -> i64 {
    vcs.store()
        .with_conn(|c| {
            Ok(
                c.query_row("SELECT count(*) FROM blob_index_model", [], |r| r.get(0))
                    .unwrap(),
            )
        })
        .unwrap()
}

#[test]
fn deferred_stale_files_are_not_counted_as_cache_hits() {
    let (_d, vcs) = repo();
    many_files(&vcs, 40);
    engine(&vcs, None)
        .ensure_revision_indexed(&main_head(), None)
        .unwrap();
    let items: Vec<_> = vcs
        .list(&main_head(), None)
        .unwrap()
        .into_iter()
        .map(|(p, b, _)| (p, b))
        .collect();
    let hybrid = MemSearchEngine::new(vcs.clone(), counting(), SearchConfig::default());
    let stats = hybrid.index_unique_blobs(&items).unwrap();
    assert_eq!(
        (stats.blobs_newly_indexed, stats.blobs_cache_hit),
        (32, 0),
        "{stats:?}"
    );
}

/// Every text maps to e1 under model "a"; under model "b" a text with "zzq" maps to e1, any
/// other text to e2.
struct Fixed(&'static str);

impl mem_search::EmbeddingProvider for Fixed {
    fn provider_name(&self) -> &str {
        self.0
    }
    fn model_id(&self) -> String {
        format!("{}:2", self.0)
    }
    fn embed_batch(&self, texts: &[String]) -> mem_core::Result<Option<Vec<Vec<f32>>>> {
        let e = |t: &String| match (self.0, t.contains("zzq")) {
            ("a", _) | (_, true) => vec![1.0, 0.0],
            _ => vec![0.0, 1.0],
        };
        Ok(Some(texts.iter().map(e).collect()))
    }
}

#[test]
fn vectors_of_another_model_are_never_scored() {
    let (_d, vcs) = repo();
    many_files(&vcs, 40);
    let with = |m| MemSearchEngine::new(vcs.clone(), Arc::new(Fixed(m)), SearchConfig::default());
    with("a")
        .ensure_revision_indexed(&main_head(), None)
        .unwrap();
    // Model "b": one search re-embeds 32 files (vector e2); 8 keep model-"a" vectors (e1).
    let hits = with("b")
        .search(&SearchRequest::new(main_head(), "zzq").with_top_k(50))
        .unwrap();
    assert!(
        hits.is_empty(),
        "stale model-a vectors matched: {:?}",
        hits.len()
    );
}

#[test]
fn reindex_without_an_active_model_refuses_to_drop_vectors() {
    let (_d, vcs) = repo();
    many_files(&vcs, 3);
    engine(&vcs, Some(16)).reindex_all().unwrap();
    let before = index_state(&vcs);
    let err = engine(&vcs, None).reindex_all().unwrap_err();
    assert!(
        err.to_string().contains("no embedding model is active"),
        "{err}"
    );
    assert_eq!(index_state(&vcs), before, "nothing was deleted");
}
