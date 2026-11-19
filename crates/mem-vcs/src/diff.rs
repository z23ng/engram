//! Merkle tree diffing with $O(1)$ subtree hash pruning and workspace overlay support.

use std::cmp::Ordering;

use mem_core::{
    BlobId, DiffEntry, DiffKind, Result, Revision, TreeEntry, TreeEntryType, TreeId, VirtualPath,
};
use mem_store::{get_tree_entries_conn, workspace_list_conn};
use rusqlite::Connection;

use crate::tree::{
    flatten_subtree_at, resolve_commit_tree_id, resolve_effective_tree, FlatTree,
};

/// Computes structured differences from `base` to `target`.
///
/// Uses Merkle subtree hash pruning when both revisions correspond directly to committed trees,
/// and falls back to effective-tree comparison when either side has staged workspace changes.
pub fn compute_diff(
    conn: &Connection,
    base: &Revision,
    target: &Revision,
) -> Result<Vec<DiffEntry>> {
    let base_dirty = revision_has_workspace_changes(conn, base)?;
    let target_dirty = revision_has_workspace_changes(conn, target)?;

    if !base_dirty && !target_dirty {
        let base_tree_id = resolve_commit_tree_id(conn, base)?;
        let target_tree_id = resolve_commit_tree_id(conn, target)?;
        let mut diffs = Vec::new();
        diff_trees(conn, None, &base_tree_id, &target_tree_id, &mut diffs)?;
        diffs.sort_by(|a, b| a.path.cmp(&b.path));
        return Ok(diffs);
    }

    let base_map = resolve_effective_tree(conn, base, None)?;
    let target_map = resolve_effective_tree(conn, target, None)?;
    Ok(diff_flat_maps(&base_map, &target_map))
}

fn revision_has_workspace_changes(conn: &Connection, rev: &Revision) -> Result<bool> {
    match rev {
        Revision::BranchWorkspace(branch) => Ok(!workspace_list_conn(conn, branch)?.is_empty()),
        Revision::BranchHead(_) | Revision::Commit(_) => Ok(false),
    }
}

/// Recursively diffs two Merkle trees, skipping identical subtrees in $O(1)$.
pub fn diff_trees(
    conn: &Connection,
    prefix: Option<&str>,
    base_tree_id: &TreeId,
    target_tree_id: &TreeId,
    out: &mut Vec<DiffEntry>,
) -> Result<()> {
    if base_tree_id == target_tree_id {
        return Ok(());
    }

    let base_entries = get_tree_entries_conn(conn, base_tree_id)?;
    let target_entries = get_tree_entries_conn(conn, target_tree_id)?;

    let mut i = 0;
    let mut j = 0;

    while i < base_entries.len() && j < target_entries.len() {
        let b = &base_entries[i];
        let t = &target_entries[j];
        match b.name.cmp(&t.name) {
            Ordering::Less => {
                emit_subtree_entries(conn, prefix, b, DiffKind::Deleted, out)?;
                i += 1;
            }
            Ordering::Greater => {
                emit_subtree_entries(conn, prefix, t, DiffKind::Added, out)?;
                j += 1;
            }
            Ordering::Equal => {
                if b == t {
                    i += 1;
                    j += 1;
                    continue;
                }
                let child_path = join_segment(prefix, &b.name);
                match (b.entry_type, t.entry_type) {
                    (TreeEntryType::Tree, TreeEntryType::Tree) => {
                        let b_tree = TreeId::from_hex(&b.child_id)?;
                        let t_tree = TreeId::from_hex(&t.child_id)?;
                        diff_trees(conn, Some(&child_path), &b_tree, &t_tree, out)?;
                    }
                    (TreeEntryType::Blob, TreeEntryType::Blob) => {
                        out.push(DiffEntry {
                            path: VirtualPath::new(child_path)?,
                            kind: DiffKind::Modified,
                            old_blob_id: Some(BlobId::from_hex(&b.child_id)?),
                            new_blob_id: Some(BlobId::from_hex(&t.child_id)?),
                            old_meta_json: Some(b.meta_json.clone()),
                            new_meta_json: Some(t.meta_json.clone()),
                        });
                    }
                    (TreeEntryType::Blob, TreeEntryType::Tree)
                    | (TreeEntryType::Tree, TreeEntryType::Blob) => {
                        emit_subtree_entries(conn, prefix, b, DiffKind::Deleted, out)?;
                        emit_subtree_entries(conn, prefix, t, DiffKind::Added, out)?;
                    }
                }
                i += 1;
                j += 1;
            }
        }
    }

    while i < base_entries.len() {
        emit_subtree_entries(conn, prefix, &base_entries[i], DiffKind::Deleted, out)?;
        i += 1;
    }

    while j < target_entries.len() {
        emit_subtree_entries(conn, prefix, &target_entries[j], DiffKind::Added, out)?;
        j += 1;
    }

    Ok(())
}

fn emit_subtree_entries(
    conn: &Connection,
    prefix: Option<&str>,
    entry: &TreeEntry,
    kind: DiffKind,
    out: &mut Vec<DiffEntry>,
) -> Result<()> {
    let full = join_segment(prefix, &entry.name);
    match entry.entry_type {
        TreeEntryType::Blob => {
            let blob_id = BlobId::from_hex(&entry.child_id)?;
            out.push(make_leaf_diff(
                VirtualPath::new(full)?,
                kind,
                blob_id,
                entry.meta_json.clone(),
            ));
        }
        TreeEntryType::Tree => {
            let sub_tree_id = TreeId::from_hex(&entry.child_id)?;
            let flat = flatten_subtree_at(conn, &sub_tree_id, &full)?;
            for (abs_path, (blob_id, meta_json)) in flat {
                out.push(make_leaf_diff(abs_path, kind, blob_id, meta_json));
            }
        }
    }
    Ok(())
}

fn make_leaf_diff(
    path: VirtualPath,
    kind: DiffKind,
    blob_id: BlobId,
    meta_json: String,
) -> DiffEntry {
    match kind {
        DiffKind::Added => DiffEntry {
            path,
            kind,
            old_blob_id: None,
            new_blob_id: Some(blob_id),
            old_meta_json: None,
            new_meta_json: Some(meta_json),
        },
        DiffKind::Deleted => DiffEntry {
            path,
            kind,
            old_blob_id: Some(blob_id),
            new_blob_id: None,
            old_meta_json: Some(meta_json),
            new_meta_json: None,
        },
        DiffKind::Modified => unreachable!("leaf helper only used for Added and Deleted"),
    }
}

fn join_segment(prefix: Option<&str>, name: &str) -> String {
    match prefix {
        Some(p) => format!("{p}/{name}"),
        None => format!("/{name}"),
    }
}

fn diff_flat_maps(base: &FlatTree, target: &FlatTree) -> Vec<DiffEntry> {
    let mut diffs = Vec::new();

    for (path, (old_blob, old_meta)) in base {
        match target.get(path) {
            None => diffs.push(DiffEntry {
                path: path.clone(),
                kind: DiffKind::Deleted,
                old_blob_id: Some(*old_blob),
                new_blob_id: None,
                old_meta_json: Some(old_meta.clone()),
                new_meta_json: None,
            }),
            Some((new_blob, new_meta)) if new_blob != old_blob || new_meta != old_meta => {
                diffs.push(DiffEntry {
                    path: path.clone(),
                    kind: DiffKind::Modified,
                    old_blob_id: Some(*old_blob),
                    new_blob_id: Some(*new_blob),
                    old_meta_json: Some(old_meta.clone()),
                    new_meta_json: Some(new_meta.clone()),
                });
            }
            Some(_) => {}
        }
    }

    for (path, (new_blob, new_meta)) in target {
        if !base.contains_key(path) {
            diffs.push(DiffEntry {
                path: path.clone(),
                kind: DiffKind::Added,
                old_blob_id: None,
                new_blob_id: Some(*new_blob),
                old_meta_json: None,
                new_meta_json: Some(new_meta.clone()),
            });
        }
    }

    diffs.sort_by(|a, b| a.path.cmp(&b.path));
    diffs
}
