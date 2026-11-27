//! Mark-and-sweep garbage collector for unreachable commits, trees, and blobs.

use std::collections::{HashSet, VecDeque};

use mem_core::{BlobId, CommitId, GcStats, MemError, Result, TreeEntryType, TreeId};
use mem_store::{get_commit_conn, get_tree_entries_conn, list_refs_conn};
use rusqlite::{params, Connection};

/// Marks all objects reachable from `refs` and `workspace_entries`, then sweeps unreachable
/// commits, trees, and blobs within a single transaction.
pub fn run_gc(conn: &Connection) -> Result<GcStats> {
    let mut reachable_commits: HashSet<CommitId> = HashSet::new();
    let mut reachable_trees: HashSet<TreeId> = HashSet::new();
    let mut reachable_blobs: HashSet<BlobId> = HashSet::new();

    // 1. Mark reachable commits from all branch refs.
    let mut commit_queue: VecDeque<CommitId> = VecDeque::new();
    for (_, target_commit) in list_refs_conn(conn)? {
        if reachable_commits.insert(target_commit) {
            commit_queue.push_back(target_commit);
        }
    }

    let mut root_trees: Vec<TreeId> = Vec::new();
    while let Some(cid) = commit_queue.pop_front() {
        let info =
            get_commit_conn(conn, &cid)?.ok_or_else(|| MemError::CommitNotFound(cid.to_hex()))?;
        root_trees.push(info.tree_id);
        for parent in info.parent_ids {
            if reachable_commits.insert(parent) {
                commit_queue.push_back(parent);
            }
        }
    }

    // 2. Mark reachable Merkle trees and leaf blobs, skipping already-visited subtrees.
    let mut tree_queue: VecDeque<TreeId> = VecDeque::new();
    for tree_id in root_trees {
        if reachable_trees.insert(tree_id) {
            tree_queue.push_back(tree_id);
        }
    }

    while let Some(tid) = tree_queue.pop_front() {
        for entry in get_tree_entries_conn(conn, &tid)? {
            match entry.entry_type {
                TreeEntryType::Blob => {
                    reachable_blobs.insert(BlobId::from_hex(&entry.child_id)?);
                }
                TreeEntryType::Tree => {
                    let sub_id = TreeId::from_hex(&entry.child_id)?;
                    if reachable_trees.insert(sub_id) {
                        tree_queue.push_back(sub_id);
                    }
                }
            }
        }
    }

    // 3. Mark blobs referenced by staged workspace entries.
    {
        let mut stmt = conn
            .prepare_cached("SELECT blob_id FROM workspace_entries WHERE blob_id IS NOT NULL")
            .map_err(|e| MemError::Storage(format!("failed to prepare workspace gc scan: {e}")))?;
        let rows = stmt
            .query_map([], |r| r.get::<_, String>(0))
            .map_err(|e| MemError::Storage(format!("failed to scan workspace blobs: {e}")))?;
        for row in rows {
            let hex = row
                .map_err(|e| MemError::Storage(format!("failed to decode workspace blob: {e}")))?;
            reachable_blobs.insert(BlobId::from_hex(&hex)?);
        }
    }

    // 4. Sweep in foreign-key safe order: commits -> tree_objects (cascades trees) -> blobs.
    let unreachable_commits = collect_unreachable_ids(conn, "commits", "commit_id", |hex| {
        CommitId::from_hex(hex).map(|id| reachable_commits.contains(&id))
    })?;
    let unreachable_trees = collect_unreachable_ids(conn, "tree_objects", "tree_id", |hex| {
        TreeId::from_hex(hex).map(|id| reachable_trees.contains(&id))
    })?;
    let unreachable_blobs = collect_unreachable_blobs(conn, &reachable_blobs)?;

    delete_by_ids(conn, "commits", "commit_id", &unreachable_commits)?;
    if !unreachable_commits.is_empty() {
        conn.execute(
            "DELETE FROM rewind_events WHERE abandoned_commit_id NOT IN (SELECT commit_id FROM commits)",
            [],
        )
        .map_err(|e| MemError::Storage(format!("failed to clean orphaned rewind_events: {e}")))?;
    }
    delete_by_ids(conn, "tree_objects", "tree_id", &unreachable_trees)?;

    let mut reclaimed_bytes: u64 = 0;
    {
        let mut stmt = conn
            .prepare_cached("DELETE FROM blobs WHERE blob_id = ?1")
            .map_err(|e| MemError::Storage(format!("failed to prepare blob delete: {e}")))?;
        for (blob_hex, stored_bytes) in &unreachable_blobs {
            stmt.execute(params![blob_hex])
                .map_err(|e| MemError::Storage(format!("failed to delete blob: {e}")))?;
            reclaimed_bytes += *stored_bytes;
        }
    }

    if !unreachable_blobs.is_empty() {
        conn.execute(
            "DELETE FROM fts_chunks WHERE blob_id NOT IN (SELECT blob_id FROM blobs)",
            [],
        )
        .map_err(|e| MemError::Storage(format!("failed to clean orphaned fts_chunks: {e}")))?;
    }

    Ok(GcStats {
        removed_commits: unreachable_commits.len(),
        removed_trees: unreachable_trees.len(),
        removed_blobs: unreachable_blobs.len(),
        reclaimed_bytes,
    })
}

fn collect_unreachable_ids<F>(
    conn: &Connection,
    table: &str,
    col: &str,
    is_reachable: F,
) -> Result<Vec<String>>
where
    F: Fn(&str) -> Result<bool>,
{
    let sql = format!("SELECT {col} FROM {table}");
    let mut stmt = conn
        .prepare(&sql)
        .map_err(|e| MemError::Storage(format!("failed to query {table} for gc: {e}")))?;
    let rows = stmt
        .query_map([], |r| r.get::<_, String>(0))
        .map_err(|e| MemError::Storage(format!("failed to scan {table} for gc: {e}")))?;

    let mut unreachable = Vec::new();
    for row in rows {
        let hex =
            row.map_err(|e| MemError::Storage(format!("failed to decode {table} id: {e}")))?;
        if !is_reachable(&hex)? {
            unreachable.push(hex);
        }
    }
    Ok(unreachable)
}

fn collect_unreachable_blobs(
    conn: &Connection,
    reachable_blobs: &HashSet<BlobId>,
) -> Result<Vec<(String, u64)>> {
    let mut stmt = conn
        .prepare_cached("SELECT blob_id, LENGTH(content) FROM blobs")
        .map_err(|e| MemError::Storage(format!("failed to prepare blob gc scan: {e}")))?;
    let rows = stmt
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))
        .map_err(|e| MemError::Storage(format!("failed to scan blobs for gc: {e}")))?;

    let mut unreachable = Vec::new();
    for row in rows {
        let (hex, len) =
            row.map_err(|e| MemError::Storage(format!("failed to decode blob gc row: {e}")))?;
        let id = BlobId::from_hex(&hex)?;
        if !reachable_blobs.contains(&id) {
            let bytes = u64::try_from(len.max(0)).unwrap_or(0);
            unreachable.push((hex, bytes));
        }
    }
    Ok(unreachable)
}

fn delete_by_ids(conn: &Connection, table: &str, col: &str, ids: &[String]) -> Result<()> {
    if ids.is_empty() {
        return Ok(());
    }
    let sql = format!("DELETE FROM {table} WHERE {col} = ?1");
    let mut stmt = conn
        .prepare(&sql)
        .map_err(|e| MemError::Storage(format!("failed to prepare {table} gc delete: {e}")))?;
    for id in ids {
        stmt.execute(params![id])
            .map_err(|e| MemError::Storage(format!("failed to gc {table} row: {e}")))?;
    }
    Ok(())
}
