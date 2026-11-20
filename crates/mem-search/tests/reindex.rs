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
