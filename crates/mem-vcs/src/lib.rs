//! Git/LayerFS-inspired Merkle tree version control kernel for AI Memory.

pub mod diff;
pub mod gc;
pub mod merge;
pub mod tree;

use std::collections::{HashSet, VecDeque};

use chrono::Utc;
use mem_core::{
    BlobId, CommitId, CommitInfo, DiffEntry, GcStats, MemError, MemoryMetadata, MemoryNode,
    MergeOutcome, MergeStrategy, Result, Revision, TreeEntryType, TreeId, VirtualPath,
};
use mem_store::cas::{get_blob_conn, put_blob_conn};
use mem_store::{
    delete_ref_conn, get_commit_conn, get_ref_conn, get_tree_entry_conn, put_commit_conn,
    put_tree_conn, set_ref_conn, workspace_clear_conn, workspace_delete_conn, workspace_get_conn,
    workspace_list_conn, workspace_list_descendants_conn, workspace_remove_entry_conn,
    workspace_upsert_conn, SqliteStore, WorkspaceOp,
};
use rusqlite::Connection;

use crate::diff::compute_diff;
use crate::gc::run_gc;
use crate::merge::merge_branches;
use crate::tree::{
    apply_workspace_overlay, build_merkle_tree, flatten_subtree_at, flatten_tree,
    flatten_tree_under, lookup_in_tree, parse_metadata, resolve_commit_tree_id,
    resolve_effective_tree,
};

/// Core version control operations on a structured memory repository.
pub trait VersionControlEngine: Send + Sync {
    /// Initializes the repository and ensures the default `main` branch exists.
    fn init(&self) -> Result<CommitId>;

    /// Writes or updates a memory node inside `branch`'s working workspace.
    fn write(&self, branch: &str, node: MemoryNode) -> Result<()>;

    /// Deletes a file or recursively deletes a directory inside `branch`'s working workspace.
    fn delete(&self, branch: &str, path: &VirtualPath) -> Result<()>;

    /// Reads a memory node at `path` from a branch workspace, branch head, or historical commit.
    fn read(&self, rev: &Revision, path: &VirtualPath) -> Result<Option<MemoryNode>>;

    /// Snapshots `branch`'s working workspace into an immutable Merkle tree and commit.
    fn commit(&self, branch: &str, author: &str, message: &str) -> Result<CommitId>;

    /// Creates a zero-copy branch pointing to the commit referenced by `from_rev`.
    fn create_branch(&self, new_branch: &str, from_rev: &Revision) -> Result<()>;

    /// Atomically resets `branch` to `target` commit and discards uncommitted workspace changes.
    fn rewind(&self, branch: &str, target: &CommitId) -> Result<()>;

    /// Computes structured differences between `base` and `target` revisions.
    fn diff(&self, base: &Revision, target: &Revision) -> Result<Vec<DiffEntry>>;

    /// Performs a three-way merge from `source_branch` into `target_branch`.
    fn merge(
        &self,
        source_branch: &str,
        target_branch: &str,
        strategy: MergeStrategy,
    ) -> Result<MergeOutcome>;

    /// Reclaims unreachable commits, Merkle trees, and blobs from storage.
    fn gc(&self) -> Result<GcStats>;
}

/// SQLite-backed implementation of [`VersionControlEngine`].
#[derive(Clone, Debug)]
pub struct MemVcs {
    store: SqliteStore,
}

impl MemVcs {
    /// Creates a new [`MemVcs`] engine backed by `store`.
    pub fn new(store: SqliteStore) -> Self {
        Self { store }
    }

    /// Returns a reference to the underlying [`SqliteStore`].
    pub fn store(&self) -> &SqliteStore {
        &self.store
    }

    /// Writes `node` into `branch`'s working workspace and returns its content-addressed [`BlobId`].
    pub fn write_node(&self, branch: &str, node: MemoryNode) -> Result<BlobId> {
        node.path.validate_file_path()?;
        let meta_json = node.metadata.canonical_json()?;

        self.store.with_tx(|tx| {
            let head_id = get_ref_conn(tx, branch)?
                .ok_or_else(|| MemError::BranchNotFound(branch.to_string()))?;
            let root_tree_id = get_commit_conn(tx, &head_id)?
                .ok_or_else(|| MemError::CommitNotFound(head_id.to_hex()))?
                .tree_id;

            let blob_id = put_blob_conn(tx, &node.content, &node.metadata.content_type)?;

            // Resolve file/directory conflicts in O(depth + subtree + workspace):
            // 1. Any active strict ancestor file (e.g. writing /semantic/a/b.md when /semantic/a is a file)
            // 2. Any active strict descendant file (e.g. writing /semantic/dir when /semantic/dir/x.md exists)
            resolve_write_conflicts(tx, branch, &root_tree_id, &node.path)?;

            workspace_upsert_conn(tx, branch, &node.path, &blob_id, &meta_json)?;
            Ok(blob_id)
        })
    }

    /// Deletes a file or directory prefix on `branch`'s working workspace, returning count removed.
    pub fn delete_path(&self, branch: &str, path: &VirtualPath) -> Result<usize> {
        self.store.with_tx(|tx| {
            let head_id = get_ref_conn(tx, branch)?
                .ok_or_else(|| MemError::BranchNotFound(branch.to_string()))?;
            let root_tree_id = get_commit_conn(tx, &head_id)?
                .ok_or_else(|| MemError::CommitNotFound(head_id.to_hex()))?
                .tree_id;

            let committed_matches = flatten_tree_under(tx, &root_tree_id, Some(path))?;

            let mut ws_matches = Vec::new();
            if let Some(exact) = workspace_get_conn(tx, branch, path)? {
                ws_matches.push(exact);
            }
            ws_matches.extend(workspace_list_descendants_conn(tx, branch, path)?);

            let mut deleted_paths: HashSet<VirtualPath> = HashSet::new();
            let mut already_tombstoned: HashSet<VirtualPath> = HashSet::new();

            for ws in ws_matches {
                if committed_matches.contains_key(&ws.path) {
                    if ws.op == WorkspaceOp::Delete {
                        already_tombstoned.insert(ws.path);
                    } else {
                        workspace_delete_conn(tx, branch, &ws.path)?;
                        already_tombstoned.insert(ws.path.clone());
                        deleted_paths.insert(ws.path);
                    }
                } else if ws.op == WorkspaceOp::Upsert {
                    workspace_remove_entry_conn(tx, branch, &ws.path)?;
                    deleted_paths.insert(ws.path);
                }
            }

            for committed_path in committed_matches.keys() {
                if !already_tombstoned.contains(committed_path) {
                    workspace_delete_conn(tx, branch, committed_path)?;
                    deleted_paths.insert(committed_path.clone());
                }
            }

            Ok(deleted_paths.len())
        })
    }

    /// Lists all active `(VirtualPath, BlobId, MemoryMetadata)` entries visible at `rev`.
    pub fn list(
        &self,
        rev: &Revision,
        prefix: Option<&VirtualPath>,
    ) -> Result<Vec<(VirtualPath, BlobId, MemoryMetadata)>> {
        self.store.with_conn(|conn| {
            let flat = resolve_effective_tree(conn, rev, prefix)?;
            let mut result = Vec::with_capacity(flat.len());
            for (path, (blob_id, meta_json)) in flat {
                let metadata = parse_metadata(&meta_json)?;
                result.push((path, blob_id, metadata));
            }
            Ok(result)
        })
    }

    /// Returns up to `limit` commits reachable from `branch`'s head in reverse-chronological BFS order.
    pub fn log(&self, branch: &str, limit: usize) -> Result<Vec<CommitInfo>> {
        self.store.with_conn(|conn| {
            let head_id = get_ref_conn(conn, branch)?
                .ok_or_else(|| MemError::BranchNotFound(branch.to_string()))?;

            if limit == 0 {
                return Ok(Vec::new());
            }

            let mut visited = HashSet::new();
            let mut queue = VecDeque::new();
            let mut commits = Vec::new();

            visited.insert(head_id);
            queue.push_back(head_id);

            while let Some(cid) = queue.pop_front() {
                if commits.len() >= limit {
                    break;
                }
                let info = get_commit_conn(conn, &cid)?
                    .ok_or_else(|| MemError::CommitNotFound(cid.to_hex()))?;
                for parent in &info.parent_ids {
                    if visited.insert(*parent) {
                        queue.push_back(*parent);
                    }
                }
                commits.push(info);
            }

            Ok(commits)
        })
    }

    /// Deletes a non-main branch reference and clears its working workspace.
    pub fn delete_branch(&self, branch: &str) -> Result<()> {
        if branch == "main" {
            return Err(MemError::CannotDeleteMainBranch);
        }
        self.store.with_tx(|tx| {
            if !delete_ref_conn(tx, branch)? {
                return Err(MemError::BranchNotFound(branch.to_string()));
            }
            workspace_clear_conn(tx, branch)?;
            Ok(())
        })
    }

    /// Returns the current [`CommitId`] targeted by `branch`.
    /// Same as [`VersionControlEngine::merge`], with `author` on the merge commit.
    pub fn merge_as(
        &self,
        source_branch: &str,
        target_branch: &str,
        strategy: MergeStrategy,
        author: &str,
    ) -> Result<MergeOutcome> {
        self.store
            .with_tx(|tx| merge_branches(tx, source_branch, target_branch, strategy, author))
    }

    pub fn head_commit(&self, branch: &str) -> Result<CommitId> {
        self.store.with_conn(|conn| {
            get_ref_conn(conn, branch)?.ok_or_else(|| MemError::BranchNotFound(branch.to_string()))
        })
    }
}

impl VersionControlEngine for MemVcs {
    fn init(&self) -> Result<CommitId> {
        self.store.with_tx(|tx| {
            if let Some(existing) = get_ref_conn(tx, "main")? {
                return Ok(existing);
            }
            let empty_tree = put_tree_conn(tx, &[])?;
            let commit_id = put_commit_conn(
                tx,
                &empty_tree,
                &[],
                "system",
                "initialize memory repository",
                Utc::now().timestamp_millis(),
            )?;
            set_ref_conn(tx, "main", &commit_id)?;
            Ok(commit_id)
        })
    }

    fn write(&self, branch: &str, node: MemoryNode) -> Result<()> {
        self.write_node(branch, node)?;
        Ok(())
    }

    fn delete(&self, branch: &str, path: &VirtualPath) -> Result<()> {
        self.delete_path(branch, path)?;
        Ok(())
    }

    fn read(&self, rev: &Revision, path: &VirtualPath) -> Result<Option<MemoryNode>> {
        self.store.with_conn(|conn| {
            let root_tree_id = resolve_commit_tree_id(conn, rev)?;

            let entry = if let Revision::BranchWorkspace(branch) = rev {
                match workspace_get_conn(conn, branch, path)? {
                    Some(ws) => match ws.op {
                        WorkspaceOp::Delete => return Ok(None),
                        WorkspaceOp::Upsert => {
                            let blob_id = ws.blob_id.ok_or_else(|| {
                                MemError::Storage(format!(
                                    "workspace upsert missing blob_id at {path}"
                                ))
                            })?;
                            let meta_json = ws.meta_json.unwrap_or_else(|| "{}".to_string());
                            Some((blob_id, meta_json))
                        }
                    },
                    None => lookup_in_tree(conn, &root_tree_id, path)?,
                }
            } else {
                lookup_in_tree(conn, &root_tree_id, path)?
            };

            let Some((blob_id, meta_json)) = entry else {
                return Ok(None);
            };
            let (content, _content_type) = get_blob_conn(conn, &blob_id)?
                .ok_or_else(|| MemError::BlobNotFound(blob_id.to_hex()))?;
            let metadata = parse_metadata(&meta_json)?;

            Ok(Some(MemoryNode {
                path: path.clone(),
                content,
                metadata,
                blob_id: Some(blob_id),
            }))
        })
    }

    fn commit(&self, branch: &str, author: &str, message: &str) -> Result<CommitId> {
        self.store.with_tx(|tx| {
            let head_id = get_ref_conn(tx, branch)?
                .ok_or_else(|| MemError::BranchNotFound(branch.to_string()))?;
            let head_commit = get_commit_conn(tx, &head_id)?
                .ok_or_else(|| MemError::CommitNotFound(head_id.to_hex()))?;

            let ws_entries = workspace_list_conn(tx, branch)?;
            let new_tree_id = if ws_entries.is_empty() {
                head_commit.tree_id
            } else {
                let mut flat = flatten_tree(tx, &head_commit.tree_id)?;
                apply_workspace_overlay(&mut flat, &ws_entries)?;
                build_merkle_tree(tx, &flat)?
            };

            let commit_id = put_commit_conn(
                tx,
                &new_tree_id,
                &[head_id],
                author,
                message,
                Utc::now().timestamp_millis(),
            )?;
            set_ref_conn(tx, branch, &commit_id)?;
            workspace_clear_conn(tx, branch)?;
            Ok(commit_id)
        })
    }

    fn create_branch(&self, new_branch: &str, from_rev: &Revision) -> Result<()> {
        if new_branch.trim().is_empty() {
            return Err(MemError::InvalidPath(
                "branch name cannot be empty".to_string(),
            ));
        }

        self.store.with_tx(|tx| {
            if get_ref_conn(tx, new_branch)?.is_some() {
                return Err(MemError::BranchAlreadyExists(new_branch.to_string()));
            }

            let target_commit = match from_rev {
                Revision::BranchWorkspace(src) | Revision::BranchHead(src) => {
                    get_ref_conn(tx, src)?.ok_or_else(|| MemError::BranchNotFound(src.clone()))?
                }
                Revision::Commit(cid) => {
                    if get_commit_conn(tx, cid)?.is_none() {
                        return Err(MemError::CommitNotFound(cid.to_hex()));
                    }
                    *cid
                }
            };

            set_ref_conn(tx, new_branch, &target_commit)
        })
    }

    fn rewind(&self, branch: &str, target: &CommitId) -> Result<()> {
        self.store.with_tx(|tx| {
            if get_ref_conn(tx, branch)?.is_none() {
                return Err(MemError::BranchNotFound(branch.to_string()));
            }
            if get_commit_conn(tx, target)?.is_none() {
                return Err(MemError::CommitNotFound(target.to_hex()));
            }
            set_ref_conn(tx, branch, target)?;
            workspace_clear_conn(tx, branch)?;
            Ok(())
        })
    }

    fn diff(&self, base: &Revision, target: &Revision) -> Result<Vec<DiffEntry>> {
        self.store
            .with_conn(|conn| compute_diff(conn, base, target))
    }

    fn merge(
        &self,
        source_branch: &str,
        target_branch: &str,
        strategy: MergeStrategy,
    ) -> Result<MergeOutcome> {
        self.merge_as(source_branch, target_branch, strategy, "mem-vcs")
    }

    fn gc(&self) -> Result<GcStats> {
        self.store.with_tx(|tx| run_gc(tx))
    }
}

/// Clears any active strict ancestor file or strict descendant files in `branch` so `path` can be
/// written without directory/file collisions.
fn resolve_write_conflicts(
    conn: &Connection,
    branch: &str,
    root_tree_id: &TreeId,
    target_path: &VirtualPath,
) -> Result<()> {
    let segments = target_path.segments();
    let mut current_tree: Option<TreeId> = Some(*root_tree_id);
    let mut committed_descendants_root: Option<TreeId> = None;

    // 1. Walk committed tree along target_path segments in O(depth).
    for (idx, seg) in segments.iter().enumerate() {
        let Some(tid) = current_tree else {
            break;
        };
        let Some(entry) = get_tree_entry_conn(conn, &tid, seg)? else {
            break;
        };

        let is_last = idx + 1 == segments.len();
        if !is_last {
            match entry.entry_type {
                TreeEntryType::Blob => {
                    let anc_str = format!("/{}", segments[..=idx].join("/"));
                    let anc_path = VirtualPath::new(anc_str)?;
                    let deleted_in_ws = matches!(
                        workspace_get_conn(conn, branch, &anc_path)?,
                        Some(ws) if ws.op == WorkspaceOp::Delete
                    );
                    if !deleted_in_ws {
                        workspace_delete_conn(conn, branch, &anc_path)?;
                    }
                    break;
                }
                TreeEntryType::Tree => {
                    current_tree = Some(TreeId::from_hex(&entry.child_id)?);
                }
            }
        } else if entry.entry_type == TreeEntryType::Tree {
            committed_descendants_root = Some(TreeId::from_hex(&entry.child_id)?);
        }
    }

    // 2. If target_path was a committed directory, tombstone all committed files beneath it.
    let mut committed_descendants = HashSet::new();
    if let Some(sub_tree_id) = committed_descendants_root {
        let sub_flat = flatten_subtree_at(conn, &sub_tree_id, target_path.as_str())?;
        for abs_path in sub_flat.into_keys() {
            workspace_delete_conn(conn, branch, &abs_path)?;
            committed_descendants.insert(abs_path);
        }
    }

    // 3. Clean up conflicting workspace entries in O(depth) ancestors + O(log N) descendants.
    let mut ancestor_opt = target_path.parent();
    while let Some(anc_path) = ancestor_opt {
        if let Some(ws) = workspace_get_conn(conn, branch, &anc_path)? {
            if ws.op == WorkspaceOp::Upsert {
                if lookup_in_tree(conn, root_tree_id, &ws.path)?.is_some() {
                    workspace_delete_conn(conn, branch, &ws.path)?;
                } else {
                    workspace_remove_entry_conn(conn, branch, &ws.path)?;
                }
            }
        }
        ancestor_opt = anc_path.parent();
    }

    for ws in workspace_list_descendants_conn(conn, branch, target_path)? {
        if ws.op != WorkspaceOp::Upsert || committed_descendants.contains(&ws.path) {
            continue;
        }
        if lookup_in_tree(conn, root_tree_id, &ws.path)?.is_some() {
            workspace_delete_conn(conn, branch, &ws.path)?;
        } else {
            workspace_remove_entry_conn(conn, branch, &ws.path)?;
        }
    }

    Ok(())
}
