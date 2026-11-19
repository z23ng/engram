//! Version control data types: revisions, Merkle entries, commits, diffs, merges, and GC stats.

use serde::{Deserialize, Serialize};

use crate::{BlobId, CommitId, MemError, Result, TreeId, VirtualPath};

/// Target view selector for read, diff, and branch creation operations.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Revision {
    /// Active view of a branch: latest commit overlaid with uncommitted workspace edits.
    BranchWorkspace(String),
    /// Immutable committed snapshot at the tip of a branch.
    BranchHead(String),
    /// Exact immutable historical commit snapshot.
    Commit(CommitId),
}

/// Kind of child referenced by a Merkle tree entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TreeEntryType {
    /// Leaf memory content blob.
    Blob,
    /// Nested subdirectory Merkle tree.
    Tree,
}

impl TreeEntryType {
    /// Returns the string representation stored in SQLite (`"blob"` or `"tree"`).
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Blob => "blob",
            Self::Tree => "tree",
        }
    }

    /// Parses `"blob"` or `"tree"` from SQLite storage.
    pub fn parse(raw: &str) -> Result<Self> {
        match raw {
            "blob" => Ok(Self::Blob),
            "tree" => Ok(Self::Tree),
            other => Err(MemError::Storage(format!(
                "invalid tree entry_type '{other}'"
            ))),
        }
    }
}

/// Single named child entry inside a Merkle tree node.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TreeEntry {
    /// Path segment name (e.g., `"core"` or `"rules.md"`).
    pub name: String,
    /// Whether `child_id` references a blob or a child tree.
    pub entry_type: TreeEntryType,
    /// Hex-encoded [`BlobId`] or [`TreeId`].
    pub child_id: String,
    /// Canonical JSON metadata for leaf blobs, or `""` for directory subtrees.
    pub meta_json: String,
}

/// Immutable commit metadata record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitInfo {
    /// Content-addressed BLAKE3 hash of this commit.
    pub commit_id: CommitId,
    /// Root Merkle tree hash captured by this commit.
    pub tree_id: TreeId,
    /// Ordered parent commit hashes (empty for initial root commit; 2 entries for merge commits).
    pub parent_ids: Vec<CommitId>,
    /// Author identifier (agent ID, user, or consolidator).
    pub author: String,
    /// Human-readable commit summary.
    pub message: String,
    /// Commit creation timestamp in Unix milliseconds.
    pub created_at_ms: i64,
}

/// Classification of change between two revisions at a virtual path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DiffKind {
    /// Path exists in target but not in base.
    Added,
    /// Path exists in both base and target with different blob or metadata.
    Modified,
    /// Path exists in base but not in target.
    Deleted,
}

/// Structured difference for a single virtual memory file path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffEntry {
    /// File path affected by the change.
    pub path: VirtualPath,
    /// Type of change from base to target.
    pub kind: DiffKind,
    /// Blob hash in the base revision, if present.
    pub old_blob_id: Option<BlobId>,
    /// Blob hash in the target revision, if present.
    pub new_blob_id: Option<BlobId>,
    /// Canonical metadata JSON in the base revision, if present.
    pub old_meta_json: Option<String>,
    /// Canonical metadata JSON in the target revision, if present.
    pub new_meta_json: Option<String>,
}

/// Conflict resolution policy when performing a three-way branch merge.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MergeStrategy {
    /// Automatically merge non-conflicting paths and report conflicts on divergent edits.
    Auto,
    /// Prefer the target (`ours`) branch when both sides modify the same path differently.
    Ours,
    /// Prefer the source (`theirs`) branch when both sides modify the same path differently.
    Theirs,
}

/// Conflicting path details returned when `MergeStrategy::Auto` hits divergent edits.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MergeConflict {
    /// Conflicting file path.
    pub path: VirtualPath,
    /// Blob hash at the lowest common ancestor, if the file existed at base.
    pub base_blob_id: Option<BlobId>,
    /// Blob hash on the target (`ours`) branch, if present.
    pub ours_blob_id: Option<BlobId>,
    /// Blob hash on the source (`theirs`) branch, if present.
    pub theirs_blob_id: Option<BlobId>,
}

/// Outcome of a three-way merge between a source branch and a target branch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum MergeOutcome {
    /// Target branch pointer advanced directly without needing a merge commit.
    FastForward(CommitId),
    /// A new merge commit combining both branches was created on target.
    Merged(CommitId),
    /// Merge aborted cleanly because one or more paths had conflicting edits.
    Conflict {
        /// Lowest common ancestor commit used as merge base.
        base_commit: CommitId,
        /// Per-path conflict records.
        conflicts: Vec<MergeConflict>,
    },
}

/// Summary of unreachable objects reclaimed by mark-and-sweep garbage collection.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GcStats {
    /// Number of unreachable commits removed.
    pub removed_commits: usize,
    /// Number of unreachable Merkle tree nodes removed.
    pub removed_trees: usize,
    /// Number of unreachable blobs removed.
    pub removed_blobs: usize,
    /// Total raw or stored bytes reclaimed from `blobs`.
    pub reclaimed_bytes: u64,
}
