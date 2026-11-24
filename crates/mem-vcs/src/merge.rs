//! Three-way merge engine with DAG LCA resolution and conflict detection.

use std::collections::hash_map::Entry;
use std::collections::{BTreeSet, HashMap, VecDeque};

use chrono::Utc;
use mem_core::{
    BlobId, CommitId, MemError, MemoryMetadata, MemoryTier, MergeConflict, MergeOutcome,
    MergeStrategy, Result, VirtualPath,
};
use mem_store::cas::{get_blob_conn, put_blob_conn};
use mem_store::{
    get_commit_conn, get_ref_conn, put_commit_conn, set_ref_conn, workspace_clear_conn,
};
use rusqlite::Connection;

use crate::tree::{build_merkle_tree, flatten_tree, parse_metadata, FlatTree};

/// Merges `source_branch` into `target_branch` inside an active transaction.
pub fn merge_branches(
    conn: &Connection,
    source_branch: &str,
    target_branch: &str,
    strategy: MergeStrategy,
    author: &str,
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
                if path.tier() == MemoryTier::Episodic {
                    if let (Some((ours_blob, ours_meta)), Some((theirs_blob, theirs_meta))) =
                        (ours_val, theirs_val)
                    {
                        if let Some(appended) = try_append_merge_episodic(
                            conn,
                            base_val.map(|(id, _)| id),
                            ours_blob,
                            ours_meta,
                            theirs_blob,
                            theirs_meta,
                        )? {
                            merged.insert(path, appended);
                            continue;
                        }
                    }
                }
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
        author,
        &message,
        Utc::now().timestamp_millis(),
    )?;

    set_ref_conn(conn, target_branch, &merge_commit)?;
    workspace_clear_conn(conn, target_branch)?;

    Ok(MergeOutcome::Merged(merge_commit))
}

fn try_append_merge_episodic(
    conn: &Connection,
    base_blob_id: Option<&BlobId>,
    ours_blob_id: &BlobId,
    ours_meta_json: &str,
    theirs_blob_id: &BlobId,
    theirs_meta_json: &str,
) -> Result<Option<(BlobId, String)>> {
    let Some((ours_bytes, ours_ct)) = get_blob_conn(conn, ours_blob_id)? else {
        return Ok(None);
    };
    let Some((theirs_bytes, _)) = get_blob_conn(conn, theirs_blob_id)? else {
        return Ok(None);
    };

    let Ok(ours_text) = std::str::from_utf8(&ours_bytes) else {
        return Ok(None);
    };
    let Ok(theirs_text) = std::str::from_utf8(&theirs_bytes) else {
        return Ok(None);
    };

    let base_text = match base_blob_id {
        Some(bid) => match get_blob_conn(conn, bid)? {
            Some((bytes, _)) => match String::from_utf8(bytes) {
                Ok(s) => Some(s),
                Err(_) => return Ok(None),
            },
            None => None,
        },
        None => None,
    };

    let ours_meta = parse_metadata(ours_meta_json)?;
    let theirs_meta = parse_metadata(theirs_meta_json)?;

    let (first_text, second_text) = if theirs_meta.updated_at_ms < ours_meta.updated_at_ms {
        (theirs_text, ours_text)
    } else {
        (ours_text, theirs_text)
    };

    let merged_text = merge_append_lines(base_text.as_deref(), first_text, second_text);

    let mut merged_tags = ours_meta.tags.clone();
    for tag in &theirs_meta.tags {
        if !merged_tags.contains(tag) {
            merged_tags.push(tag.clone());
        }
    }

    let merged_meta = MemoryMetadata {
        content_type: if ours_meta.content_type.is_empty() {
            ours_ct
        } else {
            ours_meta.content_type
        },
        importance: ours_meta.importance.max(theirs_meta.importance),
        tags: merged_tags,
        created_at_ms: ours_meta.created_at_ms.min(theirs_meta.created_at_ms),
        updated_at_ms: ours_meta.updated_at_ms.max(theirs_meta.updated_at_ms),
        ttl_seconds: ours_meta.ttl_seconds.or(theirs_meta.ttl_seconds),
    };

    let merged_meta_json = merged_meta.canonical_json()?;
    let merged_blob_id = put_blob_conn(conn, merged_text.as_bytes(), &merged_meta.content_type)?;
    Ok(Some((merged_blob_id, merged_meta_json)))
}

fn merge_append_lines(base: Option<&str>, first: &str, second: &str) -> String {
    let base_lines: Vec<&str> = base.map(|b| b.lines().collect()).unwrap_or_default();
    let first_lines: Vec<&str> = first.lines().collect();
    let second_lines: Vec<&str> = second.lines().collect();

    let mut result: Vec<String> = Vec::new();

    if !base_lines.is_empty()
        && first_lines.starts_with(base_lines.as_slice())
        && second_lines.starts_with(base_lines.as_slice())
    {
        for line in &base_lines {
            result.push((*line).to_string());
        }
        for line in &first_lines[base_lines.len()..] {
            if !result.iter().any(|existing| existing == line) || line.trim().is_empty() {
                result.push((*line).to_string());
            }
        }
        for line in &second_lines[base_lines.len()..] {
            if !result.iter().any(|existing| existing == line) || line.trim().is_empty() {
                result.push((*line).to_string());
            }
        }
    } else {
        let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
        for line in base_lines
            .into_iter()
            .chain(first_lines)
            .chain(second_lines)
        {
            if line.trim().is_empty() {
                if result.last().is_some_and(|prev| !prev.is_empty()) {
                    result.push(String::new());
                }
            } else if seen.insert(line) {
                result.push(line.to_string());
            }
        }
    }

    let mut out = result.join("\n");
    if (first.ends_with('\n') || second.ends_with('\n')) && !out.ends_with('\n') {
        out.push('\n');
    }
    out
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
