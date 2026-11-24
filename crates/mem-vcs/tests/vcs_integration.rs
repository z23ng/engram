use std::time::{Duration, Instant};

use mem_core::{
    DiffKind, MemError, MemoryMetadata, MemoryNode, MergeOutcome, MergeStrategy, Revision,
    VirtualPath,
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

#[test]
fn cas_deduplicates_identical_content_across_paths_and_branches() {
    let store = SqliteStore::open_in_memory().unwrap();
    let vcs = MemVcs::new(store.clone());
    vcs.init().unwrap();

    let shared_body = "Invariant: never leak unmerged branch memories.";
    vcs.write(
        "main",
        make_node("/core/rule_a.md", shared_body, 1.0, vec!["core"]),
    )
    .unwrap();
    vcs.write(
        "main",
        make_node("/semantic/rule_copy.md", shared_body, 0.7, vec!["copy"]),
    )
    .unwrap();
    vcs.commit("main", "agent-1", "add duplicate content across paths")
        .unwrap();

    vcs.create_branch("exp/branch-1", &Revision::BranchHead("main".into()))
        .unwrap();
    vcs.write(
        "exp/branch-1",
        make_node("/procedural/sop.md", shared_body, 0.8, vec!["sop"]),
    )
    .unwrap();
    vcs.commit("exp/branch-1", "agent-2", "same content on branch")
        .unwrap();

    assert_eq!(store.blob_count().unwrap(), 1);

    let read_main = vcs
        .read(&Revision::BranchHead("main".into()), &vp("/core/rule_a.md"))
        .unwrap()
        .unwrap();
    let read_branch = vcs
        .read(
            &Revision::BranchHead("exp/branch-1".into()),
            &vp("/procedural/sop.md"),
        )
        .unwrap()
        .unwrap();
    assert_eq!(read_main.content_str().unwrap(), shared_body);
    assert_eq!(read_branch.content_str().unwrap(), shared_body);
    assert_eq!(read_main.blob_id, read_branch.blob_id);
}

#[test]
fn zero_copy_branch_under_1ms_with_10000_nodes() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("bench_10k.db");
    let store = SqliteStore::open(&db_path).unwrap();
    let vcs = MemVcs::new(store.clone());
    vcs.init().unwrap();

    // Populate 10,000 nodes inside a single batch of workspace writes via transactions,
    // exercising the same underlying storage structures, then commit into Merkle tree.
    let tiers = ["core", "episodic", "semantic", "procedural"];
    store
        .with_tx(|tx| {
            let meta_json = MemoryMetadata {
                content_type: "text/markdown".to_string(),
                importance: 0.5,
                tags: vec!["bench".to_string()],
                created_at_ms: 1_700_000_000_000,
                updated_at_ms: 1_700_000_000_000,
                ttl_seconds: None,
            }
            .canonical_json()?;

            for i in 0..10_000usize {
                let tier = tiers[i % tiers.len()];
                let bucket = i / 100;
                let path = VirtualPath::new(format!("/{tier}/bucket_{bucket:03}/node_{i:05}.md"))?;
                let payload = format!("memory node #{i} payload content");
                let blob_id =
                    mem_store::cas::put_blob_conn(tx, payload.as_bytes(), "text/markdown")?;
                mem_store::workspace_upsert_conn(tx, "main", &path, &blob_id, &meta_json)?;
            }
            Ok(())
        })
        .unwrap();

    vcs.commit("main", "bench", "commit 10,000 memory nodes")
        .unwrap();

    let blobs_before = store.blob_count().unwrap();
    let trees_before = store.tree_count().unwrap();
    assert_eq!(blobs_before, 10_000);

    // Best of 5 runs: one wall-clock sample is noisy when the test suite runs in parallel.
    let mut elapsed = Duration::MAX;
    for name in ["subagent/bench", "b1", "b2", "b3", "b4"] {
        let start = Instant::now();
        vcs.create_branch(name, &Revision::BranchHead("main".into()))
            .unwrap();
        elapsed = elapsed.min(start.elapsed());
    }

    assert!(
        elapsed < Duration::from_millis(1),
        "expected zero-copy branch creation < 1ms, took {elapsed:?}"
    );
    assert_eq!(store.blob_count().unwrap(), blobs_before);
    assert_eq!(store.tree_count().unwrap(), trees_before);

    // Point-read on the new branch resolves accurately in O(depth).
    let sample = vcs
        .read(
            &Revision::BranchWorkspace("subagent/bench".into()),
            &vp("/semantic/bucket_042/node_04242.md"),
        )
        .unwrap()
        .unwrap();
    assert_eq!(
        sample.content_str().unwrap(),
        "memory node #4242 payload content"
    );
}

#[test]
fn rewind_boundary_scenarios_and_consecutive_rollbacks() {
    let store = SqliteStore::open_in_memory().unwrap();
    let vcs = MemVcs::new(store.clone());
    vcs.init().unwrap();

    // c1: initial files and nested directory
    vcs.write("main", make_node("/semantic/a", "a as file", 0.6, vec![]))
        .unwrap();
    vcs.write(
        "main",
        make_node("/semantic/dir/x.md", "file x", 0.5, vec![]),
    )
    .unwrap();
    vcs.write(
        "main",
        make_node("/semantic/dir/sub/y.md", "file y", 0.5, vec![]),
    )
    .unwrap();
    let c1 = vcs
        .commit("main", "agent", "c1: base files and dir")
        .unwrap();

    // c2: directory overwrites file (/semantic/a -> /semantic/a/b.md)
    vcs.write(
        "main",
        make_node("/semantic/a/b.md", "b inside dir a", 0.7, vec![]),
    )
    .unwrap();
    assert!(vcs
        .read(
            &Revision::BranchWorkspace("main".into()),
            &vp("/semantic/a")
        )
        .unwrap()
        .is_none());
    assert_eq!(
        vcs.read(
            &Revision::BranchWorkspace("main".into()),
            &vp("/semantic/a/b.md")
        )
        .unwrap()
        .unwrap()
        .content_str()
        .unwrap(),
        "b inside dir a"
    );
    let c2 = vcs
        .commit("main", "agent", "c2: dir overwrote file /semantic/a")
        .unwrap();

    // c3: file overwrites directory (/semantic/dir/x.md & /semantic/dir/sub/y.md -> /semantic/dir)
    vcs.write(
        "main",
        make_node("/semantic/dir", "dir as file", 0.8, vec![]),
    )
    .unwrap();
    assert!(vcs
        .read(
            &Revision::BranchWorkspace("main".into()),
            &vp("/semantic/dir/x.md")
        )
        .unwrap()
        .is_none());
    assert!(vcs
        .read(
            &Revision::BranchWorkspace("main".into()),
            &vp("/semantic/dir/sub/y.md")
        )
        .unwrap()
        .is_none());
    let c3 = vcs
        .commit("main", "agent", "c3: file overwrote /semantic/dir")
        .unwrap();

    // c4: nested directory delete of /semantic/a and modify /semantic/dir
    vcs.delete("main", &vp("/semantic/a")).unwrap();
    vcs.write("main", make_node("/semantic/dir", "dir v2", 0.8, vec![]))
        .unwrap();
    let _c4 = vcs
        .commit(
            "main",
            "agent",
            "c4: deleted /semantic/a dir and modified /semantic/dir",
        )
        .unwrap();

    // Add uncommitted dirty workspace pollution before rewinding
    vcs.write(
        "main",
        make_node("/core/hallucination.md", "bad memory", 1.0, vec![]),
    )
    .unwrap();

    // Rewind 1: back to c3 -> dirty workspace dropped, /semantic/a/b.md and /semantic/dir file restored
    vcs.rewind("main", &c3).unwrap();
    assert!(store.workspace_list("main").unwrap().is_empty());
    assert!(vcs
        .read(
            &Revision::BranchWorkspace("main".into()),
            &vp("/core/hallucination.md")
        )
        .unwrap()
        .is_none());
    assert_eq!(
        vcs.read(
            &Revision::BranchWorkspace("main".into()),
            &vp("/semantic/a/b.md")
        )
        .unwrap()
        .unwrap()
        .content_str()
        .unwrap(),
        "b inside dir a"
    );
    assert_eq!(
        vcs.read(
            &Revision::BranchWorkspace("main".into()),
            &vp("/semantic/dir")
        )
        .unwrap()
        .unwrap()
        .content_str()
        .unwrap(),
        "dir as file"
    );

    // Rewind 2: back to c2 -> /semantic/dir is a directory again with x.md and sub/y.md
    vcs.rewind("main", &c2).unwrap();
    assert!(vcs
        .read(
            &Revision::BranchWorkspace("main".into()),
            &vp("/semantic/dir")
        )
        .unwrap()
        .is_none());
    assert_eq!(
        vcs.read(
            &Revision::BranchWorkspace("main".into()),
            &vp("/semantic/dir/sub/y.md")
        )
        .unwrap()
        .unwrap()
        .content_str()
        .unwrap(),
        "file y"
    );

    // Rewind 3: back to c1 -> /semantic/a is a file again and /semantic/a/b.md is gone
    vcs.rewind("main", &c1).unwrap();
    assert_eq!(
        vcs.read(
            &Revision::BranchWorkspace("main".into()),
            &vp("/semantic/a")
        )
        .unwrap()
        .unwrap()
        .content_str()
        .unwrap(),
        "a as file"
    );
    assert!(vcs
        .read(
            &Revision::BranchWorkspace("main".into()),
            &vp("/semantic/a/b.md")
        )
        .unwrap()
        .is_none());

    // Nested directory deletion on c1 followed by rewind back to c1
    let removed = vcs.delete_path("main", &vp("/semantic/dir")).unwrap();
    assert_eq!(removed, 2);
    let c_after_del = vcs.commit("main", "agent", "delete /semantic/dir").unwrap();
    assert_ne!(c_after_del, c1);
    assert!(vcs
        .list(
            &Revision::BranchWorkspace("main".into()),
            Some(&vp("/semantic/dir"))
        )
        .unwrap()
        .is_empty());

    vcs.rewind("main", &c1).unwrap();
    let restored = vcs
        .list(
            &Revision::BranchWorkspace("main".into()),
            Some(&vp("/semantic/dir")),
        )
        .unwrap();
    assert_eq!(restored.len(), 2);
}

#[test]
fn branch_isolation_diff_merge_and_gc() {
    let store = SqliteStore::open_in_memory().unwrap();
    let vcs = MemVcs::new(store.clone());
    let c0 = vcs.init().unwrap();

    vcs.write(
        "main",
        make_node("/core/goal.md", "ship v1", 1.0, vec!["goal"]),
    )
    .unwrap();
    vcs.write(
        "main",
        make_node("/semantic/arch.md", "sqlite wal", 0.8, vec!["arch"]),
    )
    .unwrap();
    let c1 = vcs
        .commit("main", "main-agent", "initial core and arch")
        .unwrap();

    vcs.create_branch("sub/a", &Revision::BranchHead("main".into()))
        .unwrap();
    vcs.create_branch("sub/b", &Revision::BranchHead("main".into()))
        .unwrap();

    // Branch sub/a adds /procedural/tip.md and modifies /semantic/arch.md
    vcs.write(
        "sub/a",
        make_node("/procedural/tip.md", "use blake3", 0.9, vec!["tip"]),
    )
    .unwrap();
    vcs.write(
        "sub/a",
        make_node("/semantic/arch.md", "sqlite wal + zstd", 0.85, vec!["arch"]),
    )
    .unwrap();
    vcs.commit("sub/a", "subagent-a", "sub/a findings").unwrap();

    // Branch sub/b writes secret/unmerged knowledge + conflicting /semantic/arch.md
    vcs.write(
        "sub/b",
        make_node("/semantic/secret_b.md", "only in branch b", 0.5, vec![]),
    )
    .unwrap();
    vcs.write(
        "sub/b",
        make_node("/semantic/arch.md", "postgres", 0.8, vec!["arch"]),
    )
    .unwrap();
    vcs.commit("sub/b", "subagent-b", "sub/b divergence")
        .unwrap();

    // Strict branch isolation: main must not see sub/a or sub/b nodes
    assert!(vcs
        .read(
            &Revision::BranchWorkspace("main".into()),
            &vp("/semantic/secret_b.md")
        )
        .unwrap()
        .is_none());
    assert!(vcs
        .read(
            &Revision::BranchWorkspace("main".into()),
            &vp("/procedural/tip.md")
        )
        .unwrap()
        .is_none());
    assert_eq!(
        vcs.list(&Revision::BranchWorkspace("main".into()), None)
            .unwrap()
            .len(),
        2
    );

    // Diff main vs sub/a
    let diffs = vcs
        .diff(
            &Revision::BranchHead("main".into()),
            &Revision::BranchHead("sub/a".into()),
        )
        .unwrap();
    assert_eq!(diffs.len(), 2);
    assert_eq!(diffs[0].path, vp("/procedural/tip.md"));
    assert_eq!(diffs[0].kind, DiffKind::Added);
    assert_eq!(diffs[1].path, vp("/semantic/arch.md"));
    assert_eq!(diffs[1].kind, DiffKind::Modified);

    // Fast-forward merge sub/a into main
    let ff = vcs.merge("sub/a", "main", MergeStrategy::Auto).unwrap();
    assert!(matches!(ff, MergeOutcome::FastForward(_)));
    assert_eq!(
        vcs.read(
            &Revision::BranchHead("main".into()),
            &vp("/procedural/tip.md")
        )
        .unwrap()
        .unwrap()
        .content_str()
        .unwrap(),
        "use blake3"
    );

    // Conflict detection when merging divergent sub/b into main with Auto
    let conflict = vcs.merge("sub/b", "main", MergeStrategy::Auto).unwrap();
    match conflict {
        MergeOutcome::Conflict {
            base_commit,
            conflicts,
        } => {
            assert_eq!(base_commit, c1);
            assert_eq!(conflicts.len(), 1);
            assert_eq!(conflicts[0].path, vp("/semantic/arch.md"));
        }
        other => panic!("expected Conflict, got {other:?}"),
    }

    // Resolve with MergeStrategy::Ours -> keeps sqlite wal + zstd and imports secret_b.md
    let merged = vcs.merge("sub/b", "main", MergeStrategy::Ours).unwrap();
    assert!(matches!(merged, MergeOutcome::Merged(_)));
    assert_eq!(
        vcs.read(
            &Revision::BranchHead("main".into()),
            &vp("/semantic/arch.md")
        )
        .unwrap()
        .unwrap()
        .content_str()
        .unwrap(),
        "sqlite wal + zstd"
    );
    assert_eq!(
        vcs.read(
            &Revision::BranchHead("main".into()),
            &vp("/semantic/secret_b.md")
        )
        .unwrap()
        .unwrap()
        .content_str()
        .unwrap(),
        "only in branch b"
    );

    // Log traverses DAG history back to root commit c0
    let history = vcs.log("main", 10).unwrap();
    assert!(history.iter().any(|c| c.commit_id == c0));
    assert!(history.iter().any(|c| c.commit_id == c1));

    // Delete main branch is rejected
    assert!(matches!(
        vcs.delete_branch("main"),
        Err(MemError::CannotDeleteMainBranch)
    ));

    // Rewind main back to c1, delete sub/a and sub/b, then run GC
    vcs.rewind("main", &c1).unwrap();
    vcs.delete_branch("sub/a").unwrap();
    vcs.delete_branch("sub/b").unwrap();

    // Stage an uncommitted workspace blob on main to verify GC preserves workspace blobs
    vcs.write(
        "main",
        make_node("/episodic/staged.md", "keep staged blob", 0.5, vec![]),
    )
    .unwrap();

    let gc_stats = vcs.gc().unwrap();
    assert!(gc_stats.removed_commits >= 3);
    assert!(gc_stats.removed_trees >= 1);
    assert!(gc_stats.removed_blobs >= 3);
    assert!(gc_stats.reclaimed_bytes > 0);

    // Main reachable blobs and staged workspace blob remain intact after GC
    assert_eq!(
        vcs.read(&Revision::BranchHead("main".into()), &vp("/core/goal.md"))
            .unwrap()
            .unwrap()
            .content_str()
            .unwrap(),
        "ship v1"
    );
    assert_eq!(
        vcs.read(
            &Revision::BranchWorkspace("main".into()),
            &vp("/episodic/staged.md")
        )
        .unwrap()
        .unwrap()
        .content_str()
        .unwrap(),
        "keep staged blob"
    );
}

#[test]
fn uncommitted_workspace_transitions_workspace_diff_and_merge_hierarchy_conflict() {
    let store = SqliteStore::open_in_memory().unwrap();
    let vcs = MemVcs::new(store.clone());
    vcs.init().unwrap();

    // Uncommitted write -> dir over file -> file over dir before committing
    vcs.write("main", make_node("/semantic/x", "file x", 0.5, vec![]))
        .unwrap();
    vcs.write(
        "main",
        make_node("/semantic/x/child.md", "nested child", 0.5, vec![]),
    )
    .unwrap();
    assert!(vcs
        .read(
            &Revision::BranchWorkspace("main".into()),
            &vp("/semantic/x")
        )
        .unwrap()
        .is_none());
    vcs.write(
        "main",
        make_node("/semantic/x", "file x again", 0.6, vec![]),
    )
    .unwrap();
    assert!(vcs
        .read(
            &Revision::BranchWorkspace("main".into()),
            &vp("/semantic/x/child.md")
        )
        .unwrap()
        .is_none());

    // Diff between BranchHead (empty) and dirty BranchWorkspace shows only /semantic/x
    let ws_diff = vcs
        .diff(
            &Revision::BranchHead("main".into()),
            &Revision::BranchWorkspace("main".into()),
        )
        .unwrap();
    assert_eq!(ws_diff.len(), 1);
    assert_eq!(ws_diff[0].path, vp("/semantic/x"));
    assert_eq!(ws_diff[0].kind, DiffKind::Added);

    let c1 = vcs.commit("main", "agent", "commit /semantic/x").unwrap();

    // Diverge two branches with a cross-branch file vs directory collision
    vcs.create_branch("dir_side", &Revision::Commit(c1))
        .unwrap();
    vcs.write("main", make_node("/semantic/node", "as file", 0.7, vec![]))
        .unwrap();
    vcs.commit("main", "agent", "main creates file /semantic/node")
        .unwrap();

    vcs.write(
        "dir_side",
        make_node("/semantic/node/leaf.md", "as dir", 0.7, vec![]),
    )
    .unwrap();
    vcs.commit(
        "dir_side",
        "agent",
        "dir_side creates /semantic/node/leaf.md",
    )
    .unwrap();

    let conflict = vcs.merge("dir_side", "main", MergeStrategy::Auto).unwrap();
    assert!(matches!(conflict, MergeOutcome::Conflict { .. }));

    let merged_theirs = vcs
        .merge("dir_side", "main", MergeStrategy::Theirs)
        .unwrap();
    assert!(matches!(merged_theirs, MergeOutcome::Merged(_)));
    assert!(vcs
        .read(&Revision::BranchHead("main".into()), &vp("/semantic/node"))
        .unwrap()
        .is_none());
    assert_eq!(
        vcs.read(
            &Revision::BranchHead("main".into()),
            &vp("/semantic/node/leaf.md")
        )
        .unwrap()
        .unwrap()
        .content_str()
        .unwrap(),
        "as dir"
    );
}

#[test]
fn concurrent_threads_on_isolated_branches_succeed_without_leakage() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("concurrent.db");
    let store = SqliteStore::open(&db_path).unwrap();
    let vcs = MemVcs::new(store);
    vcs.init().unwrap();

    vcs.write(
        "main",
        make_node("/core/shared.md", "base rule", 1.0, vec![]),
    )
    .unwrap();
    vcs.commit("main", "init", "base commit").unwrap();

    let mut handles = Vec::new();
    for worker_id in 0..4usize {
        let branch = format!("worker/w{worker_id}");
        vcs.create_branch(&branch, &Revision::BranchHead("main".into()))
            .unwrap();
        let vcs_clone = vcs.clone();
        handles.push(std::thread::spawn(move || {
            for step in 0..25usize {
                let path = format!("/episodic/w{worker_id}/step_{step:02}.md");
                let body = format!("worker {worker_id} step {step}");
                vcs_clone
                    .write(&branch, make_node(&path, &body, 0.5, vec![]))
                    .unwrap();
            }
            vcs_clone
                .commit(&branch, &format!("worker-{worker_id}"), "batch write")
                .unwrap()
        }));
    }

    for handle in handles {
        handle.join().unwrap();
    }

    // Verify main remains completely untouched and each worker branch has 26 nodes (1 shared + 25 own).
    assert_eq!(
        vcs.list(&Revision::BranchHead("main".into()), None)
            .unwrap()
            .len(),
        1
    );
    for worker_id in 0..4usize {
        let branch = format!("worker/w{worker_id}");
        let entries = vcs
            .list(&Revision::BranchHead(branch.clone()), None)
            .unwrap();
        assert_eq!(entries.len(), 26);
        assert!(vcs
            .read(
                &Revision::BranchHead(branch),
                &vp(&format!("/episodic/w{}/step_00.md", (worker_id + 1) % 4))
            )
            .unwrap()
            .is_none());
    }
}

#[test]
fn episodic_append_only_auto_merges_while_semantic_conflicts() {
    let store = SqliteStore::open_in_memory().unwrap();
    let vcs = MemVcs::new(store);
    vcs.init().unwrap();

    vcs.write(
        "main",
        make_node(
            "/episodic/2026-11-24.md",
            "- 09:00 session started\n",
            0.5,
            vec!["log"],
        ),
    )
    .unwrap();
    vcs.write(
        "main",
        make_node("/semantic/policy.md", "Base policy", 0.8, vec!["policy"]),
    )
    .unwrap();
    vcs.commit("main", "init", "seed base logs and semantic card")
        .unwrap();

    vcs.create_branch("agent/a", &Revision::BranchHead("main".into()))
        .unwrap();
    vcs.create_branch("agent/b", &Revision::BranchHead("main".into()))
        .unwrap();

    // Both branches append to the same episodic timeline log (agent/a earlier, agent/b later).
    let mut node_a = make_node(
        "/episodic/2026-11-24.md",
        "- 09:00 session started\n- 09:05 agent/a ran benchmark\n",
        0.6,
        vec!["log", "bench"],
    );
    node_a.metadata.updated_at_ms = 1_700_000_100_000;
    vcs.write("agent/a", node_a).unwrap();
    vcs.commit("agent/a", "a", "agent a log").unwrap();

    let mut node_b = make_node(
        "/episodic/2026-11-24.md",
        "- 09:00 session started\n- 09:10 agent/b verified sqlite wal\n",
        0.7,
        vec!["log", "wal"],
    );
    node_b.metadata.updated_at_ms = 1_700_000_200_000;
    vcs.write("agent/b", node_b).unwrap();
    vcs.commit("agent/b", "b", "agent b log").unwrap();

    assert!(matches!(
        vcs.merge("agent/a", "main", MergeStrategy::Auto).unwrap(),
        MergeOutcome::FastForward(_)
    ));
    assert!(matches!(
        vcs.merge("agent/b", "main", MergeStrategy::Auto).unwrap(),
        MergeOutcome::Merged(_)
    ));

    let merged_log = vcs
        .read(
            &Revision::BranchHead("main".into()),
            &vp("/episodic/2026-11-24.md"),
        )
        .unwrap()
        .unwrap();
    let text = merged_log.content_str().unwrap();
    assert!(text.contains("- 09:00 session started"));
    assert!(text.contains("- 09:05 agent/a ran benchmark"));
    assert!(text.contains("- 09:10 agent/b verified sqlite wal"));
    // Ordered chronologically by updated_at_ms (agent/a before agent/b).
    assert!(text.find("agent/a").unwrap() < text.find("agent/b").unwrap());
    assert!(merged_log.metadata.tags.contains(&"bench".to_string()));
    assert!(merged_log.metadata.tags.contains(&"wal".to_string()));

    // Verify /semantic/ divergent writes STILL report structured Conflict under Auto.
    vcs.create_branch("sem/a", &Revision::BranchHead("main".into()))
        .unwrap();
    vcs.create_branch("sem/b", &Revision::BranchHead("main".into()))
        .unwrap();

    vcs.write(
        "sem/a",
        make_node("/semantic/policy.md", "Agent A policy", 0.8, vec!["policy"]),
    )
    .unwrap();
    vcs.commit("sem/a", "a", "semantic edit a").unwrap();

    vcs.write(
        "sem/b",
        make_node("/semantic/policy.md", "Agent B policy", 0.8, vec!["policy"]),
    )
    .unwrap();
    vcs.commit("sem/b", "b", "semantic edit b").unwrap();

    assert!(matches!(
        vcs.merge("sem/a", "main", MergeStrategy::Auto).unwrap(),
        MergeOutcome::FastForward(_)
    ));
    assert!(matches!(
        vcs.merge("sem/b", "main", MergeStrategy::Auto).unwrap(),
        MergeOutcome::Conflict { .. }
    ));
}
