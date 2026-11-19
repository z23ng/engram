//! SQLite-backed CAS, Merkle tree, commit, ref, and workspace storage layer.

pub mod cas;
pub mod schema;

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::{Arc, Mutex};

use chrono::Utc;
use mem_core::{
    BlobId, CommitId, CommitInfo, MemError, Result, TreeEntry, TreeEntryType, TreeId, VirtualPath,
};
use rusqlite::{params, Connection, OptionalExtension, Transaction};

/// Operation type recorded in the per-branch `workspace_entries` overlay table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkspaceOp {
    /// Insert or overwrite a file in the working workspace.
    Upsert,
    /// Tombstone marking deletion of a file present in the committed tree.
    Delete,
}

impl WorkspaceOp {
    /// Returns the string stored in SQLite (`"upsert"` or `"delete"`).
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Upsert => "upsert",
            Self::Delete => "delete",
        }
    }

    /// Parses `"upsert"` or `"delete"` from SQLite.
    pub fn parse(raw: &str) -> Result<Self> {
        match raw {
            "upsert" => Ok(Self::Upsert),
            "delete" => Ok(Self::Delete),
            other => Err(MemError::Storage(format!("invalid workspace op '{other}'"))),
        }
    }
}

/// Single staged change in a branch's working workspace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceEntry {
    /// Branch owning this staged entry.
    pub branch: String,
    /// Normalized virtual path of the staged memory file.
    pub path: VirtualPath,
    /// Staged operation (`Upsert` or `Delete`).
    pub op: WorkspaceOp,
    /// Content-addressed blob hash when `op == WorkspaceOp::Upsert`.
    pub blob_id: Option<BlobId>,
    /// Canonical metadata JSON when `op == WorkspaceOp::Upsert`.
    pub meta_json: Option<String>,
    /// Last modification timestamp in Unix milliseconds.
    pub updated_at_ms: i64,
}

/// Thread-safe handle to a single-file SQLite WAL database implementing CAS and VCS tables.
#[derive(Clone, Debug)]
pub struct SqliteStore {
    conn: Arc<Mutex<Connection>>,
}

impl SqliteStore {
    /// Opens or creates a SQLite database at `path` with WAL mode and schema initialized.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let conn = Connection::open(path)
            .map_err(|e| MemError::Storage(format!("failed to open sqlite db: {e}")))?;
        schema::initialize_database(&conn)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    /// Opens an isolated in-memory SQLite database with schema initialized (primarily for tests).
    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()
            .map_err(|e| MemError::Storage(format!("failed to open in-memory sqlite db: {e}")))?;
        schema::initialize_database(&conn)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    /// Executes a closure with shared access to the underlying SQLite connection.
    pub fn with_conn<F, T>(&self, f: F) -> Result<T>
    where
        F: FnOnce(&Connection) -> Result<T>,
    {
        let guard = self
            .conn
            .lock()
            .map_err(|_| MemError::Storage("sqlite connection mutex poisoned".to_string()))?;
        f(&guard)
    }

    /// Executes a closure inside an atomic SQLite transaction (`BEGIN IMMEDIATE` / `COMMIT`).
    pub fn with_tx<F, T>(&self, f: F) -> Result<T>
    where
        F: FnOnce(&Transaction<'_>) -> Result<T>,
    {
        let mut guard = self
            .conn
            .lock()
            .map_err(|_| MemError::Storage("sqlite connection mutex poisoned".to_string()))?;
        let tx = guard
            .transaction()
            .map_err(|e| MemError::Storage(format!("failed to begin sqlite tx: {e}")))?;
        let result = f(&tx)?;
        tx.commit()
            .map_err(|e| MemError::Storage(format!("failed to commit sqlite tx: {e}")))?;
        Ok(result)
    }

    /// Compresses and deduplicates `raw_bytes` into `blobs`, returning its [`BlobId`].
    pub fn put_blob(&self, raw_bytes: &[u8], content_type: &str) -> Result<BlobId> {
        self.with_conn(|conn| cas::put_blob_conn(conn, raw_bytes, content_type))
    }

    /// Reads and decompresses a blob from `blobs`, verifying its size and BLAKE3 digest.
    pub fn get_blob(&self, blob_id: &BlobId) -> Result<Option<(Vec<u8>, String)>> {
        self.with_conn(|conn| cas::get_blob_conn(conn, blob_id))
    }

    /// Returns `true` if `blob_id` exists in `blobs`.
    pub fn has_blob(&self, blob_id: &BlobId) -> Result<bool> {
        self.with_conn(|conn| cas::has_blob_conn(conn, blob_id))
    }

    /// Returns the number of stored blobs.
    pub fn blob_count(&self) -> Result<usize> {
        self.with_conn(cas::blob_count_conn)
    }

    /// Returns the number of stored Merkle tree nodes.
    pub fn tree_count(&self) -> Result<usize> {
        self.with_conn(cas::tree_count_conn)
    }

    /// Returns the number of stored commits.
    pub fn commit_count(&self) -> Result<usize> {
        self.with_conn(cas::commit_count_conn)
    }

    /// Stores an immutable Merkle tree node sorted by `entry.name`, returning its [`TreeId`].
    pub fn put_tree(&self, entries: &[TreeEntry]) -> Result<TreeId> {
        self.with_tx(|tx| put_tree_conn(tx, entries))
    }

    /// Reads all sorted [`TreeEntry`] children of `tree_id`.
    pub fn get_tree_entries(&self, tree_id: &TreeId) -> Result<Vec<TreeEntry>> {
        self.with_conn(|conn| get_tree_entries_conn(conn, tree_id))
    }

    /// Reads a single direct child entry named `entry_name` under `tree_id` in $O(1)$.
    pub fn get_tree_entry(&self, tree_id: &TreeId, entry_name: &str) -> Result<Option<TreeEntry>> {
        self.with_conn(|conn| get_tree_entry_conn(conn, tree_id, entry_name))
    }

    /// Stores an immutable commit object and returns its content-addressed [`CommitId`].
    pub fn put_commit(
        &self,
        tree_id: &TreeId,
        parent_ids: &[CommitId],
        author: &str,
        message: &str,
        created_at_ms: i64,
    ) -> Result<CommitId> {
        self.with_conn(|conn| {
            put_commit_conn(conn, tree_id, parent_ids, author, message, created_at_ms)
        })
    }

    /// Fetches a commit by [`CommitId`].
    pub fn get_commit(&self, commit_id: &CommitId) -> Result<Option<CommitInfo>> {
        self.with_conn(|conn| get_commit_conn(conn, commit_id))
    }

    /// Inserts or updates a branch reference to point to `target`.
    pub fn set_ref(&self, name: &str, target: &CommitId) -> Result<()> {
        self.with_conn(|conn| set_ref_conn(conn, name, target))
    }

    /// Resolves a branch reference to its current [`CommitId`].
    pub fn get_ref(&self, name: &str) -> Result<Option<CommitId>> {
        self.with_conn(|conn| get_ref_conn(conn, name))
    }

    /// Lists all branch references ordered by name.
    pub fn list_refs(&self) -> Result<Vec<(String, CommitId)>> {
        self.with_conn(list_refs_conn)
    }

    /// Deletes a branch reference. Returns `true` if a reference existed.
    pub fn delete_ref(&self, name: &str) -> Result<bool> {
        self.with_conn(|conn| delete_ref_conn(conn, name))
    }

    /// Stages a file write or overwrite in `branch`'s working workspace.
    pub fn workspace_upsert(
        &self,
        branch: &str,
        path: &VirtualPath,
        blob_id: &BlobId,
        meta_json: &str,
    ) -> Result<()> {
        self.with_conn(|conn| workspace_upsert_conn(conn, branch, path, blob_id, meta_json))
    }

    /// Stages a deletion tombstone for `path` in `branch`'s working workspace.
    pub fn workspace_delete(&self, branch: &str, path: &VirtualPath) -> Result<()> {
        self.with_conn(|conn| workspace_delete_conn(conn, branch, path))
    }

    /// Removes a workspace entry row entirely (e.g., dropping an uncommitted write).
    pub fn workspace_remove_entry(&self, branch: &str, path: &VirtualPath) -> Result<bool> {
        self.with_conn(|conn| workspace_remove_entry_conn(conn, branch, path))
    }

    /// Retrieves a single staged entry from `branch`'s working workspace.
    pub fn workspace_get(
        &self,
        branch: &str,
        path: &VirtualPath,
    ) -> Result<Option<WorkspaceEntry>> {
        self.with_conn(|conn| workspace_get_conn(conn, branch, path))
    }

    /// Lists all staged entries in `branch`'s working workspace ordered by path.
    pub fn workspace_list(&self, branch: &str) -> Result<Vec<WorkspaceEntry>> {
        self.with_conn(|conn| workspace_list_conn(conn, branch))
    }

    /// Lists staged entries in `branch`'s working workspace strictly nested under `prefix`.
    pub fn workspace_list_descendants(
        &self,
        branch: &str,
        prefix: &VirtualPath,
    ) -> Result<Vec<WorkspaceEntry>> {
        self.with_conn(|conn| workspace_list_descendants_conn(conn, branch, prefix))
    }

    /// Removes all staged workspace entries for `branch`.
    pub fn workspace_clear(&self, branch: &str) -> Result<()> {
        self.with_conn(|conn| workspace_clear_conn(conn, branch))
    }
}

fn update_len_prefixed(hasher: &mut blake3::Hasher, bytes: &[u8]) {
    hasher.update(&(bytes.len() as u64).to_le_bytes());
    hasher.update(bytes);
}

/// Inserts an immutable Merkle tree node using an existing connection or transaction.
pub fn put_tree_conn(conn: &Connection, entries: &[TreeEntry]) -> Result<TreeId> {
    let mut sorted = entries.to_vec();
    sorted.sort_by(|a, b| a.name.cmp(&b.name));

    let mut seen_names = BTreeSet::new();
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"tree:v1\0");

    for entry in &sorted {
        if !seen_names.insert(&entry.name) {
            return Err(MemError::Storage(format!(
                "duplicate tree entry name '{}'",
                entry.name
            )));
        }
        update_len_prefixed(&mut hasher, entry.entry_type.as_str().as_bytes());
        update_len_prefixed(&mut hasher, entry.name.as_bytes());
        update_len_prefixed(&mut hasher, entry.child_id.as_bytes());
        update_len_prefixed(&mut hasher, entry.meta_json.as_bytes());
    }

    let tree_id = TreeId::from_bytes(*hasher.finalize().as_bytes());
    let tree_hex = tree_id.to_hex();
    let now_ms = Utc::now().timestamp_millis();

    let inserted = conn
        .execute(
            "INSERT OR IGNORE INTO tree_objects (tree_id, created_at_ms) VALUES (?1, ?2)",
            params![tree_hex, now_ms],
        )
        .map_err(|e| MemError::Storage(format!("failed to insert tree_object: {e}")))?;

    if inserted > 0 && !sorted.is_empty() {
        let mut stmt = conn
            .prepare_cached(
                "INSERT OR IGNORE INTO trees (tree_id, entry_name, entry_type, child_id, meta_json)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
            )
            .map_err(|e| MemError::Storage(format!("failed to prepare tree insert: {e}")))?;
        for entry in &sorted {
            stmt.execute(params![
                tree_hex,
                entry.name,
                entry.entry_type.as_str(),
                entry.child_id,
                entry.meta_json,
            ])
            .map_err(|e| MemError::Storage(format!("failed to insert tree entry: {e}")))?;
        }
    }

    Ok(tree_id)
}

fn ensure_tree_exists(conn: &Connection, tree_id: &TreeId) -> Result<()> {
    let exists: Option<i64> = conn
        .query_row(
            "SELECT 1 FROM tree_objects WHERE tree_id = ?1",
            params![tree_id.to_hex()],
            |r| r.get(0),
        )
        .optional()
        .map_err(|e| MemError::Storage(format!("failed to verify tree existence: {e}")))?;

    if exists.is_none() {
        return Err(MemError::TreeNotFound(tree_id.to_hex()));
    }
    Ok(())
}

/// Reads all sorted [`TreeEntry`] records of `tree_id` using an existing connection.
pub fn get_tree_entries_conn(conn: &Connection, tree_id: &TreeId) -> Result<Vec<TreeEntry>> {
    ensure_tree_exists(conn, tree_id)?;

    let mut stmt = conn
        .prepare_cached(
            "SELECT entry_name, entry_type, child_id, meta_json
             FROM trees WHERE tree_id = ?1 ORDER BY entry_name ASC",
        )
        .map_err(|e| MemError::Storage(format!("failed to prepare tree query: {e}")))?;

    let rows = stmt
        .query_map(params![tree_id.to_hex()], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
            ))
        })
        .map_err(|e| MemError::Storage(format!("failed to query tree entries: {e}")))?;

    let mut out = Vec::new();
    for row in rows {
        let (name, entry_type_raw, child_id, meta_json) =
            row.map_err(|e| MemError::Storage(format!("failed to decode tree entry: {e}")))?;
        out.push(TreeEntry {
            name,
            entry_type: TreeEntryType::parse(&entry_type_raw)?,
            child_id,
            meta_json,
        });
    }
    Ok(out)
}

/// Reads a single named child entry from `tree_id` using an existing connection.
pub fn get_tree_entry_conn(
    conn: &Connection,
    tree_id: &TreeId,
    entry_name: &str,
) -> Result<Option<TreeEntry>> {
    ensure_tree_exists(conn, tree_id)?;

    let row: Option<(String, String, String, String)> = conn
        .query_row(
            "SELECT entry_name, entry_type, child_id, meta_json
             FROM trees WHERE tree_id = ?1 AND entry_name = ?2",
            params![tree_id.to_hex(), entry_name],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .optional()
        .map_err(|e| MemError::Storage(format!("failed to query single tree entry: {e}")))?;

    let Some((name, entry_type_raw, child_id, meta_json)) = row else {
        return Ok(None);
    };

    Ok(Some(TreeEntry {
        name,
        entry_type: TreeEntryType::parse(&entry_type_raw)?,
        child_id,
        meta_json,
    }))
}

/// Inserts an immutable commit record using an existing connection or transaction.
pub fn put_commit_conn(
    conn: &Connection,
    tree_id: &TreeId,
    parent_ids: &[CommitId],
    author: &str,
    message: &str,
    created_at_ms: i64,
) -> Result<CommitId> {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"commit:v1\0");
    let tree_hex = tree_id.to_hex();
    update_len_prefixed(&mut hasher, tree_hex.as_bytes());
    hasher.update(&(parent_ids.len() as u64).to_le_bytes());
    for parent in parent_ids {
        update_len_prefixed(&mut hasher, parent.to_hex().as_bytes());
    }
    update_len_prefixed(&mut hasher, author.as_bytes());
    update_len_prefixed(&mut hasher, message.as_bytes());
    hasher.update(&created_at_ms.to_le_bytes());

    let commit_id = CommitId::from_bytes(*hasher.finalize().as_bytes());
    let parent_json = serde_json::to_string(parent_ids)?;

    conn.execute(
        "INSERT OR IGNORE INTO commits (commit_id, tree_id, parent_ids, author, message, created_at_ms)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            commit_id.to_hex(),
            tree_hex,
            parent_json,
            author,
            message,
            created_at_ms
        ],
    )
    .map_err(|e| MemError::Storage(format!("failed to insert commit: {e}")))?;

    Ok(commit_id)
}

/// Reads a [`CommitInfo`] record by [`CommitId`] using an existing connection.
pub fn get_commit_conn(conn: &Connection, commit_id: &CommitId) -> Result<Option<CommitInfo>> {
    let mut stmt = conn
        .prepare_cached(
            "SELECT tree_id, parent_ids, author, message, created_at_ms
             FROM commits WHERE commit_id = ?1",
        )
        .map_err(|e| MemError::Storage(format!("failed to prepare commit query: {e}")))?;

    let row: Option<(String, String, String, String, i64)> = stmt
        .query_row(params![commit_id.to_hex()], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
        })
        .optional()
        .map_err(|e| MemError::Storage(format!("failed to query commit: {e}")))?;

    let Some((tree_hex, parents_json, author, message, created_at_ms)) = row else {
        return Ok(None);
    };

    let tree_id = TreeId::from_hex(&tree_hex)?;
    let parent_ids: Vec<CommitId> = serde_json::from_str(&parents_json)?;

    Ok(Some(CommitInfo {
        commit_id: *commit_id,
        tree_id,
        parent_ids,
        author,
        message,
        created_at_ms,
    }))
}

/// Upserts a branch reference using an existing connection or transaction.
pub fn set_ref_conn(conn: &Connection, name: &str, target: &CommitId) -> Result<()> {
    let now_ms = Utc::now().timestamp_millis();
    let mut stmt = conn
        .prepare_cached(
            "INSERT INTO refs (name, target_commit_id, updated_at_ms)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(name) DO UPDATE SET
                 target_commit_id = excluded.target_commit_id,
                 updated_at_ms = excluded.updated_at_ms",
        )
        .map_err(|e| MemError::Storage(format!("failed to prepare set_ref: {e}")))?;
    stmt.execute(params![name, target.to_hex(), now_ms])
        .map_err(|e| MemError::Storage(format!("failed to set ref '{name}': {e}")))?;
    Ok(())
}

/// Resolves a branch reference using an existing connection.
pub fn get_ref_conn(conn: &Connection, name: &str) -> Result<Option<CommitId>> {
    let mut stmt = conn
        .prepare_cached("SELECT target_commit_id FROM refs WHERE name = ?1")
        .map_err(|e| MemError::Storage(format!("failed to prepare get_ref: {e}")))?;
    let target: Option<String> = stmt
        .query_row(params![name], |r| r.get(0))
        .optional()
        .map_err(|e| MemError::Storage(format!("failed to query ref '{name}': {e}")))?;

    target
        .map(|hex_str| CommitId::from_hex(&hex_str))
        .transpose()
}

/// Lists all branch references sorted by name using an existing connection.
pub fn list_refs_conn(conn: &Connection) -> Result<Vec<(String, CommitId)>> {
    let mut stmt = conn
        .prepare_cached("SELECT name, target_commit_id FROM refs ORDER BY name ASC")
        .map_err(|e| MemError::Storage(format!("failed to prepare refs list: {e}")))?;
    let rows = stmt
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
        .map_err(|e| MemError::Storage(format!("failed to list refs: {e}")))?;

    let mut out = Vec::new();
    for row in rows {
        let (name, commit_hex) =
            row.map_err(|e| MemError::Storage(format!("failed to read ref row: {e}")))?;
        out.push((name, CommitId::from_hex(&commit_hex)?));
    }
    Ok(out)
}

/// Deletes a branch reference using an existing connection.
pub fn delete_ref_conn(conn: &Connection, name: &str) -> Result<bool> {
    let affected = conn
        .execute("DELETE FROM refs WHERE name = ?1", params![name])
        .map_err(|e| MemError::Storage(format!("failed to delete ref '{name}': {e}")))?;
    Ok(affected > 0)
}

/// Stages an upsert entry in `workspace_entries` using an existing connection.
pub fn workspace_upsert_conn(
    conn: &Connection,
    branch: &str,
    path: &VirtualPath,
    blob_id: &BlobId,
    meta_json: &str,
) -> Result<()> {
    let now_ms = Utc::now().timestamp_millis();
    conn.execute(
        "INSERT INTO workspace_entries (branch, path, op, blob_id, meta_json, updated_at_ms)
         VALUES (?1, ?2, 'upsert', ?3, ?4, ?5)
         ON CONFLICT(branch, path) DO UPDATE SET
             op = 'upsert',
             blob_id = excluded.blob_id,
             meta_json = excluded.meta_json,
             updated_at_ms = excluded.updated_at_ms",
        params![branch, path.as_str(), blob_id.to_hex(), meta_json, now_ms],
    )
    .map_err(|e| MemError::Storage(format!("failed to upsert workspace entry: {e}")))?;
    Ok(())
}

/// Stages a delete tombstone in `workspace_entries` using an existing connection.
pub fn workspace_delete_conn(conn: &Connection, branch: &str, path: &VirtualPath) -> Result<()> {
    let now_ms = Utc::now().timestamp_millis();
    conn.execute(
        "INSERT INTO workspace_entries (branch, path, op, blob_id, meta_json, updated_at_ms)
         VALUES (?1, ?2, 'delete', NULL, NULL, ?3)
         ON CONFLICT(branch, path) DO UPDATE SET
             op = 'delete',
             blob_id = NULL,
             meta_json = NULL,
             updated_at_ms = excluded.updated_at_ms",
        params![branch, path.as_str(), now_ms],
    )
    .map_err(|e| MemError::Storage(format!("failed to insert workspace delete tombstone: {e}")))?;
    Ok(())
}

/// Removes a workspace entry row entirely using an existing connection.
pub fn workspace_remove_entry_conn(
    conn: &Connection,
    branch: &str,
    path: &VirtualPath,
) -> Result<bool> {
    let affected = conn
        .execute(
            "DELETE FROM workspace_entries WHERE branch = ?1 AND path = ?2",
            params![branch, path.as_str()],
        )
        .map_err(|e| MemError::Storage(format!("failed to remove workspace entry: {e}")))?;
    Ok(affected > 0)
}

/// Fetches a single workspace entry using an existing connection.
pub fn workspace_get_conn(
    conn: &Connection,
    branch: &str,
    path: &VirtualPath,
) -> Result<Option<WorkspaceEntry>> {
    let row: Option<(String, Option<String>, Option<String>, i64)> = conn
        .query_row(
            "SELECT op, blob_id, meta_json, updated_at_ms
             FROM workspace_entries WHERE branch = ?1 AND path = ?2",
            params![branch, path.as_str()],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .optional()
        .map_err(|e| MemError::Storage(format!("failed to query workspace entry: {e}")))?;

    let Some((op_raw, blob_hex, meta_json, updated_at_ms)) = row else {
        return Ok(None);
    };

    Ok(Some(WorkspaceEntry {
        branch: branch.to_string(),
        path: path.clone(),
        op: WorkspaceOp::parse(&op_raw)?,
        blob_id: blob_hex.map(|h| BlobId::from_hex(&h)).transpose()?,
        meta_json,
        updated_at_ms,
    }))
}

/// Lists all workspace entries for `branch` sorted by path using an existing connection.
pub fn workspace_list_conn(conn: &Connection, branch: &str) -> Result<Vec<WorkspaceEntry>> {
    let mut stmt = conn
        .prepare_cached(
            "SELECT path, op, blob_id, meta_json, updated_at_ms
             FROM workspace_entries WHERE branch = ?1 ORDER BY path ASC",
        )
        .map_err(|e| MemError::Storage(format!("failed to prepare workspace list: {e}")))?;

    let rows = stmt
        .query_map(params![branch], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<String>>(2)?,
                r.get::<_, Option<String>>(3)?,
                r.get::<_, i64>(4)?,
            ))
        })
        .map_err(|e| MemError::Storage(format!("failed to query workspace entries: {e}")))?;

    let mut out = Vec::new();
    for row in rows {
        let (path_raw, op_raw, blob_hex, meta_json, updated_at_ms) =
            row.map_err(|e| MemError::Storage(format!("failed to decode workspace entry: {e}")))?;
        out.push(WorkspaceEntry {
            branch: branch.to_string(),
            path: VirtualPath::new(path_raw)?,
            op: WorkspaceOp::parse(&op_raw)?,
            blob_id: blob_hex.map(|h| BlobId::from_hex(&h)).transpose()?,
            meta_json,
            updated_at_ms,
        });
    }
    Ok(out)
}

/// Lists staged workspace entries strictly nested under `prefix` using an index range scan.
pub fn workspace_list_descendants_conn(
    conn: &Connection,
    branch: &str,
    prefix: &VirtualPath,
) -> Result<Vec<WorkspaceEntry>> {
    // ASCII '/' is 47 and '0' is 48, so [prefix + "/", prefix + "0") matches exact descendants.
    let start = format!("{}/", prefix.as_str());
    let end = format!("{}0", prefix.as_str());
    let mut stmt = conn
        .prepare_cached(
            "SELECT path, op, blob_id, meta_json, updated_at_ms
             FROM workspace_entries
             WHERE branch = ?1 AND path >= ?2 AND path < ?3
             ORDER BY path ASC",
        )
        .map_err(|e| MemError::Storage(format!("failed to prepare descendant query: {e}")))?;

    let rows = stmt
        .query_map(params![branch, start, end], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<String>>(2)?,
                r.get::<_, Option<String>>(3)?,
                r.get::<_, i64>(4)?,
            ))
        })
        .map_err(|e| {
            MemError::Storage(format!("failed to query descendant workspace entries: {e}"))
        })?;

    let mut out = Vec::new();
    for row in rows {
        let (path_raw, op_raw, blob_hex, meta_json, updated_at_ms) =
            row.map_err(|e| MemError::Storage(format!("failed to decode workspace entry: {e}")))?;
        out.push(WorkspaceEntry {
            branch: branch.to_string(),
            path: VirtualPath::new(path_raw)?,
            op: WorkspaceOp::parse(&op_raw)?,
            blob_id: blob_hex.map(|h| BlobId::from_hex(&h)).transpose()?,
            meta_json,
            updated_at_ms,
        });
    }
    Ok(out)
}

/// Clears all workspace entries for `branch` using an existing connection.
pub fn workspace_clear_conn(conn: &Connection, branch: &str) -> Result<()> {
    conn.execute(
        "DELETE FROM workspace_entries WHERE branch = ?1",
        params![branch],
    )
    .map_err(|e| MemError::Storage(format!("failed to clear workspace for '{branch}': {e}")))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn wal_mode_and_cas_deduplication_work() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("mem.db");
        let store = SqliteStore::open(&db_path).unwrap();

        let journal_mode: String = store
            .with_conn(|c| {
                c.query_row("PRAGMA journal_mode;", [], |r| r.get(0))
                    .map_err(|e| MemError::Storage(e.to_string()))
            })
            .unwrap();
        assert_eq!(journal_mode.to_lowercase(), "wal");

        let id1 = store
            .put_blob(b"same memory payload", "text/markdown")
            .unwrap();
        let id2 = store
            .put_blob(b"same memory payload", "text/markdown")
            .unwrap();
        assert_eq!(id1, id2);
        assert_eq!(store.blob_count().unwrap(), 1);
        assert!(store.has_blob(&id1).unwrap());

        let (bytes, ct) = store.get_blob(&id1).unwrap().unwrap();
        assert_eq!(bytes, b"same memory payload");
        assert_eq!(ct, "text/markdown");
    }

    #[test]
    fn empty_and_nonempty_trees_commits_refs_round_trip() {
        let store = SqliteStore::open_in_memory().unwrap();
        let empty_tree = store.put_tree(&[]).unwrap();
        assert_eq!(store.get_tree_entries(&empty_tree).unwrap(), vec![]);

        let missing = TreeId::hash_bytes(b"nonexistent");
        assert!(matches!(
            store.get_tree_entries(&missing),
            Err(MemError::TreeNotFound(_))
        ));

        let blob = store.put_blob(b"rule 1", "text/markdown").unwrap();
        let tree_id = store
            .put_tree(&[TreeEntry {
                name: "rules.md".to_string(),
                entry_type: TreeEntryType::Blob,
                child_id: blob.to_hex(),
                meta_json: "{}".to_string(),
            }])
            .unwrap();
        assert_eq!(
            store
                .get_tree_entry(&tree_id, "rules.md")
                .unwrap()
                .unwrap()
                .child_id,
            blob.to_hex()
        );

        let cid = store
            .put_commit(&tree_id, &[], "system", "init", 1_700_000_000_000)
            .unwrap();
        let info = store.get_commit(&cid).unwrap().unwrap();
        assert_eq!(info.tree_id, tree_id);
        assert_eq!(info.message, "init");

        store.set_ref("main", &cid).unwrap();
        assert_eq!(store.get_ref("main").unwrap(), Some(cid));
        assert_eq!(store.list_refs().unwrap(), vec![("main".to_string(), cid)]);
        assert!(store.delete_ref("main").unwrap());
        assert_eq!(store.get_ref("main").unwrap(), None);
    }
}
