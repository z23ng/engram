//! Merkle tree traversal, lookup, overlay resolution, and bottom-up construction.

use std::collections::BTreeMap;

use mem_core::{
    BlobId, CommitId, MemError, MemoryMetadata, Result, Revision, TreeEntry, TreeEntryType, TreeId,
    VirtualPath,
};
use mem_store::{
    get_commit_conn, get_ref_conn, get_tree_entries_conn, get_tree_entry_conn, put_tree_conn,
    workspace_list_conn, WorkspaceEntry, WorkspaceOp,
};
use rusqlite::Connection;

/// Flattened map of all file paths in a revision to their `(BlobId, canonical_meta_json)`.
pub type FlatTree = BTreeMap<VirtualPath, (BlobId, String)>;

/// Resolves a [`Revision`] to its base [`CommitId`] (and optional branch name for workspace overlay).
pub fn resolve_commit_id(conn: &Connection, rev: &Revision) -> Result<CommitId> {
    match rev {
        Revision::BranchWorkspace(branch) | Revision::BranchHead(branch) => {
            get_ref_conn(conn, branch)?.ok_or_else(|| MemError::BranchNotFound(branch.clone()))
        }
        Revision::Commit(commit_id) => {
            if get_commit_conn(conn, commit_id)?.is_none() {
                return Err(MemError::CommitNotFound(commit_id.to_hex()));
            }
            Ok(*commit_id)
        }
    }
}

/// Resolves the root [`TreeId`] of a [`Revision`]'s committed snapshot.
pub fn resolve_commit_tree_id(conn: &Connection, rev: &Revision) -> Result<TreeId> {
    let commit_id = resolve_commit_id(conn, rev)?;
    let commit = get_commit_conn(conn, &commit_id)?
        .ok_or_else(|| MemError::CommitNotFound(commit_id.to_hex()))?;
    Ok(commit.tree_id)
}

/// Looks up a single file path inside `root_tree_id` in $O(\text{depth})$ queries.
pub fn lookup_in_tree(
    conn: &Connection,
    root_tree_id: &TreeId,
    path: &VirtualPath,
) -> Result<Option<(BlobId, String)>> {
    let segments = path.segments();
    let mut current_tree = *root_tree_id;

    for (idx, seg) in segments.iter().enumerate() {
        let Some(entry) = get_tree_entry_conn(conn, &current_tree, seg)? else {
            return Ok(None);
        };
        let is_last = idx + 1 == segments.len();
        if is_last {
            return match entry.entry_type {
                TreeEntryType::Blob => {
                    Ok(Some((BlobId::from_hex(&entry.child_id)?, entry.meta_json)))
                }
                TreeEntryType::Tree => Ok(None),
            };
        }

        match entry.entry_type {
            TreeEntryType::Tree => {
                current_tree = TreeId::from_hex(&entry.child_id)?;
            }
            TreeEntryType::Blob => return Ok(None),
        }
    }

    Ok(None)
}

/// Walks down `prefix` in `root_tree_id` and flattens only the matching node or subtree.
pub fn flatten_tree_under(
    conn: &Connection,
    root_tree_id: &TreeId,
    prefix: Option<&VirtualPath>,
) -> Result<FlatTree> {
    let mut out = BTreeMap::new();
    let Some(prefix_path) = prefix else {
        flatten_subtree_recursive(conn, root_tree_id, None, &mut out)?;
        return Ok(out);
    };

    let segments = prefix_path.segments();
    let mut current_tree = *root_tree_id;

    for (idx, seg) in segments.iter().enumerate() {
        let Some(entry) = get_tree_entry_conn(conn, &current_tree, seg)? else {
            return Ok(out);
        };
        let is_last = idx + 1 == segments.len();
        if is_last {
            match entry.entry_type {
                TreeEntryType::Blob => {
                    out.insert(
                        prefix_path.clone(),
                        (BlobId::from_hex(&entry.child_id)?, entry.meta_json),
                    );
                }
                TreeEntryType::Tree => {
                    let sub_id = TreeId::from_hex(&entry.child_id)?;
                    flatten_subtree_recursive(conn, &sub_id, Some(prefix_path.as_str()), &mut out)?;
                }
            }
            return Ok(out);
        }

        match entry.entry_type {
            TreeEntryType::Tree => current_tree = TreeId::from_hex(&entry.child_id)?,
            TreeEntryType::Blob => return Ok(out),
        }
    }

    Ok(out)
}

/// Flattens an entire Merkle tree starting at `root_tree_id`.
pub fn flatten_tree(conn: &Connection, root_tree_id: &TreeId) -> Result<FlatTree> {
    flatten_tree_under(conn, root_tree_id, None)
}

/// Flattens a subtree `tree_id` rooted at an already-resolved virtual directory `prefix`.
pub fn flatten_subtree_at(conn: &Connection, tree_id: &TreeId, prefix: &str) -> Result<FlatTree> {
    let mut out = BTreeMap::new();
    flatten_subtree_recursive(conn, tree_id, Some(prefix), &mut out)?;
    Ok(out)
}

fn flatten_subtree_recursive(
    conn: &Connection,
    tree_id: &TreeId,
    prefix: Option<&str>,
    out: &mut FlatTree,
) -> Result<()> {
    let entries = get_tree_entries_conn(conn, tree_id)?;
    for entry in entries {
        let full = match prefix {
            Some(p) => format!("{p}/{}", entry.name),
            None => format!("/{}", entry.name),
        };
        match entry.entry_type {
            TreeEntryType::Blob => {
                let path = VirtualPath::new(full)?;
                let blob_id = BlobId::from_hex(&entry.child_id)?;
                out.insert(path, (blob_id, entry.meta_json));
            }
            TreeEntryType::Tree => {
                let sub_id = TreeId::from_hex(&entry.child_id)?;
                flatten_subtree_recursive(conn, &sub_id, Some(&full), out)?;
            }
        }
    }
    Ok(())
}

/// Applies staged workspace operations on top of a base [`FlatTree`].
pub fn apply_workspace_overlay(base: &mut FlatTree, entries: &[WorkspaceEntry]) -> Result<()> {
    for entry in entries {
        match entry.op {
            WorkspaceOp::Delete => {
                base.remove(&entry.path);
            }
            WorkspaceOp::Upsert => {
                let blob_id = entry.blob_id.ok_or_else(|| {
                    MemError::Storage(format!(
                        "workspace upsert missing blob_id at {}",
                        entry.path
                    ))
                })?;
                let meta_json = entry.meta_json.clone().unwrap_or_else(|| "{}".to_string());
                base.insert(entry.path.clone(), (blob_id, meta_json));
            }
        }
    }
    Ok(())
}

/// Resolves the effective [`FlatTree`] for any [`Revision`] (including workspace overlay if requested).
pub fn resolve_effective_tree(
    conn: &Connection,
    rev: &Revision,
    prefix: Option<&VirtualPath>,
) -> Result<FlatTree> {
    let root_tree_id = resolve_commit_tree_id(conn, rev)?;
    let mut flat = flatten_tree_under(conn, &root_tree_id, prefix)?;

    if let Revision::BranchWorkspace(branch) = rev {
        let ws_entries = workspace_list_conn(conn, branch)?;
        let filtered: Vec<WorkspaceEntry> = match prefix {
            Some(p) => ws_entries
                .into_iter()
                .filter(|e| e.path.starts_with(p))
                .collect(),
            None => ws_entries,
        };
        apply_workspace_overlay(&mut flat, &filtered)?;
    }

    Ok(flat)
}

/// Builds a bottom-up Merkle tree in SQLite from a flattened map of file paths.
pub fn build_merkle_tree(conn: &Connection, flat_entries: &FlatTree) -> Result<TreeId> {
    let owned_segments: Vec<(Vec<&str>, BlobId, &str)> = flat_entries
        .iter()
        .map(|(path, (blob_id, meta))| (path.segments(), *blob_id, meta.as_str()))
        .collect();

    let items: Vec<TreeBuildItem<'_>> = owned_segments
        .iter()
        .map(|(segs, blob_id, meta)| (segs.as_slice(), *blob_id, *meta))
        .collect();

    build_tree_level(conn, &items)
}

type TreeBuildItem<'a> = (&'a [&'a str], BlobId, &'a str);

fn build_tree_level(conn: &Connection, items: &[TreeBuildItem<'_>]) -> Result<TreeId> {
    if items.is_empty() {
        return put_tree_conn(conn, &[]);
    }

    let mut leaves: BTreeMap<&str, (BlobId, &str)> = BTreeMap::new();
    let mut subdirs: BTreeMap<&str, Vec<TreeBuildItem<'_>>> = BTreeMap::new();

    for &(segs, blob_id, meta) in items {
        let Some((&first, rest)) = segs.split_first() else {
            continue;
        };
        if rest.is_empty() {
            if subdirs.contains_key(first) {
                return Err(MemError::DirectoryFileConflict {
                    path: first.to_string(),
                    detail: "path cannot be both a file and a directory".to_string(),
                });
            }
            leaves.insert(first, (blob_id, meta));
        } else {
            if leaves.contains_key(first) {
                return Err(MemError::DirectoryFileConflict {
                    path: first.to_string(),
                    detail: "path cannot be both a directory and a file".to_string(),
                });
            }
            subdirs
                .entry(first)
                .or_default()
                .push((rest, blob_id, meta));
        }
    }

    let mut entries = Vec::with_capacity(leaves.len() + subdirs.len());
    for (name, (blob_id, meta)) in leaves {
        entries.push(TreeEntry {
            name: name.to_string(),
            entry_type: TreeEntryType::Blob,
            child_id: blob_id.to_hex(),
            meta_json: meta.to_string(),
        });
    }
    for (name, children) in subdirs {
        let child_tree_id = build_tree_level(conn, &children)?;
        entries.push(TreeEntry {
            name: name.to_string(),
            entry_type: TreeEntryType::Tree,
            child_id: child_tree_id.to_hex(),
            meta_json: String::new(),
        });
    }

    put_tree_conn(conn, &entries)
}

/// Parses canonical metadata JSON back into [`MemoryMetadata`].
pub fn parse_metadata(meta_json: &str) -> Result<MemoryMetadata> {
    Ok(serde_json::from_str(meta_json)?)
}
