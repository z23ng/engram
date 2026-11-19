//! Three-way merge engine with DAG LCA resolution and conflict detection.

use std::collections::hash_map::Entry;
use std::collections::{BTreeSet, HashMap, VecDeque};

use chrono::Utc;
use mem_core::{
    CommitId, MemError, MergeConflict, MergeOutcome, MergeStrategy, Result, VirtualPath,
};
use mem_store::{
    get_commit_conn, get_ref_conn, put_commit_conn, set_ref_conn, workspace_clear_conn,
};
use rusqlite::Connection;

use crate::tree::{build_merkle_tree, flatten_tree, FlatTree};

/// Merges `source_branch` into `target_branch` inside an active transaction.
pub fn merge_branches(
    conn: &Connection,
    source_branch: &str,
    target_branch: &str,
    strategy: MergeStrategy,
) -> Result<MergeOutcome> {
    let source_head = get_ref_conn(conn, source_branch)?
        .ok_or_else(|| MemError::BranchNotFound(source_branch.to_string()))?;
    let target_head = get_ref_conn(conn, target_branch)?
        .ok_or_else(|| MemError::BranchNotFound(target_branch.to_string()))?;

    if source_head == target_head {
        return Ok(MergeOutcome::FastForward(target_head));
    }

    let base_commit = find_lca(conn, source_head, target_head)?;
    if base_commit == target_head {
        set_ref_conn(conn, target_branch, &source_head)?;
        workspace_clear_conn(conn, target_branch)?;
        return Ok(MergeOutcome::FastForward(source_head));
    }
    if base_commit == source_head {
        return Ok(MergeOutcome::FastForward(target_head));
    }

    let base_tree_id = get_commit_conn(conn, &base_commit)?
        .ok_or_else(|| MemError::CommitNotFound(base_commit.to_hex()))?
        .tree_id;
    let ours_tree_id = get_commit_conn(conn, &target_head)?
        .ok_or_else(|| MemError::CommitNotFound(target_head.to_hex()))?
        .tree_id;
    let theirs_tree_id = get_commit_conn(conn, &source_head)?
        .ok_or_else(|| MemError::CommitNotFound(source_head.to_hex()))?
        .tree_id;

    let base_map = flatten_tree(conn, &base_tree_id)?;
    let ours_map = flatten_tree(conn, &ours_tree_id)?;
    let theirs_map = flatten_tree(conn, &theirs_tree_id)?;

    let mut all_paths: BTreeSet<VirtualPath> = BTreeSet::new();
    all_paths.extend(base_map.keys().cloned());
    all_paths.extend(ours_map.keys().cloned());
    all_paths.extend(theirs_map.keys().cloned());

    let mut merged = FlatTree::new();
    let mut conflicts = Vec::new();

    for path in all_paths {
        let base_val = base_map.get(&path);
        let ours_val = ours_map.get(&path);
        let theirs_val = theirs_map.get(&path);

        if ours_val == theirs_val {
            if let Some(val) = ours_val {
                merged.insert(path, val.clone());
            }
            continue;
        }
        if theirs_val == base_val {
            if let Some(val) = ours_val {
                merged.insert(path, val.clone());
            }
            continue;
        }
        if ours_val == base_val {
            if let Some(val) = theirs_val {
                merged.insert(path, val.clone());
            }
            continue;
        }

        match strategy {
            MergeStrategy::Ours => {
                if let Some(val) = ours_val {
                    merged.insert(path, val.clone());
                }
            }
            MergeStrategy::Theirs => {
                if let Some(val) = theirs_val {
                    merged.insert(path, val.clone());
                }
            }
            MergeStrategy::Auto => {
                conflicts.push(MergeConflict {
                    path,
                    base_blob_id: base_val.map(|(id, _)| *id),
                    ours_blob_id: ours_val.map(|(id, _)| *id),
                    theirs_blob_id: theirs_val.map(|(id, _)| *id),
                });
            }
        }
    }

    resolve_hierarchy_collisions(
        &mut merged,
        &ours_map,
        &theirs_map,
        strategy,
        &mut conflicts,
    );

    if !conflicts.is_empty() {
        conflicts.sort_by(|a, b| a.path.cmp(&b.path));
        return Ok(MergeOutcome::Conflict {
            base_commit,
            conflicts,
        });
    }

    let merged_tree_id = build_merkle_tree(conn, &merged)?;
    let message = format!("merge '{source_branch}' into '{target_branch}'");
    let merge_commit = put_commit_conn(
        conn,
        &merged_tree_id,
        &[target_head, source_head],
        "mem-vcs",
        &message,
        Utc::now().timestamp_millis(),
    )?;

    set_ref_conn(conn, target_branch, &merge_commit)?;
    workspace_clear_conn(conn, target_branch)?;

    Ok(MergeOutcome::Merged(merge_commit))
}

fn resolve_hierarchy_collisions(
    merged: &mut FlatTree,
    ours_map: &FlatTree,
    theirs_map: &FlatTree,
    strategy: MergeStrategy,
    conflicts: &mut Vec<MergeConflict>,
) {
    let paths: Vec<VirtualPath> = merged.keys().cloned().collect();
    let mut to_remove = Vec::new();

    for i in 0..paths.len() {
        for j in (i + 1)..paths.len() {
            let anc = &paths[i];
            let desc = &paths[j];
            if anc.is_strict_ancestor_of(desc) {
                match strategy {
                    MergeStrategy::Auto => {
                        let conflict_path = if theirs_map.contains_key(desc) {
                            desc.clone()
                        } else {
                            anc.clone()
                        };
                        if !conflicts.iter().any(|c| c.path == conflict_path) {
                            conflicts.push(MergeConflict {
                                path: conflict_path.clone(),
                                base_blob_id: None,
                                ours_blob_id: ours_map.get(&conflict_path).map(|(b, _)| *b),
                                theirs_blob_id: theirs_map.get(&conflict_path).map(|(b, _)| *b),
                            });
                        }
                    }
                    MergeStrategy::Ours => {
                        if ours_map.contains_key(anc) {
                            to_remove.push(desc.clone());
                        } else {
                            to_remove.push(anc.clone());
                        }
                    }
                    MergeStrategy::Theirs => {
                        if theirs_map.contains_key(anc) {
                            to_remove.push(desc.clone());
                        } else {
                            to_remove.push(anc.clone());
                        }
                    }
                }
            }
        }
    }

    for path in to_remove {
        merged.remove(&path);
    }
}

fn find_lca(conn: &Connection, source: CommitId, target: CommitId) -> Result<CommitId> {
    let target_distances = bfs_ancestors(conn, target)?;
    let source_distances = bfs_ancestors(conn, source)?;

    let mut best: Option<(usize, CommitId)> = None;
    for (commit_id, s_dist) in &source_distances {
        if let Some(t_dist) = target_distances.get(commit_id) {
            let total_dist = s_dist + t_dist;
            match best {
                None => best = Some((total_dist, *commit_id)),
                Some((best_dist, _)) if total_dist < best_dist => {
                    best = Some((total_dist, *commit_id));
                }
                _ => {}
            }
        }
    }

    best.map(|(_, cid)| cid).ok_or_else(|| {
        MemError::Storage(format!(
            "no common ancestor between commits {source} and {target}"
        ))
    })
}

fn bfs_ancestors(conn: &Connection, start: CommitId) -> Result<HashMap<CommitId, usize>> {
    let mut dists = HashMap::new();
    let mut queue = VecDeque::new();
    dists.insert(start, 0);
    queue.push_back((start, 0));

    while let Some((cid, dist)) = queue.pop_front() {
        let info =
            get_commit_conn(conn, &cid)?.ok_or_else(|| MemError::CommitNotFound(cid.to_hex()))?;
        for parent in info.parent_ids {
            if let Entry::Vacant(e) = dists.entry(parent) {
                e.insert(dist + 1);
                queue.push_back((parent, dist + 1));
            }
        }
    }

    Ok(dists)
}
