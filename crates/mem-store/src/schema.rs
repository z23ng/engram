//! SQLite WAL initialization and schema DDL for `mem-store`.

use mem_core::{MemError, Result};
use rusqlite::Connection;

/// Configures SQLite pragmas and initializes the CAS, Merkle tree, commit, ref,
/// and workspace tables idempotently.
pub fn initialize_database(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        r#"
        PRAGMA journal_mode = WAL;
        PRAGMA synchronous = NORMAL;
        PRAGMA foreign_keys = ON;
        PRAGMA busy_timeout = 5000;

        CREATE TABLE IF NOT EXISTS blobs (
            blob_id TEXT PRIMARY KEY,
            content BLOB NOT NULL,
            raw_size INTEGER NOT NULL,
            content_type TEXT NOT NULL,
            created_at_ms INTEGER NOT NULL
        );

        CREATE TABLE IF NOT EXISTS tree_objects (
            tree_id TEXT PRIMARY KEY,
            created_at_ms INTEGER NOT NULL
        );

        CREATE TABLE IF NOT EXISTS trees (
            tree_id TEXT NOT NULL REFERENCES tree_objects(tree_id) ON DELETE CASCADE,
            entry_name TEXT NOT NULL,
            entry_type TEXT NOT NULL CHECK(entry_type IN ('blob', 'tree')),
            child_id TEXT NOT NULL,
            meta_json TEXT NOT NULL,
            PRIMARY KEY (tree_id, entry_name)
        );

        CREATE TABLE IF NOT EXISTS commits (
            commit_id TEXT PRIMARY KEY,
            tree_id TEXT NOT NULL REFERENCES tree_objects(tree_id),
            parent_ids TEXT NOT NULL,
            author TEXT NOT NULL,
            message TEXT NOT NULL,
            created_at_ms INTEGER NOT NULL
        );

        CREATE TABLE IF NOT EXISTS refs (
            name TEXT PRIMARY KEY,
            target_commit_id TEXT NOT NULL REFERENCES commits(commit_id),
            updated_at_ms INTEGER NOT NULL
        );

        CREATE TABLE IF NOT EXISTS workspace_entries (
            branch TEXT NOT NULL,
            path TEXT NOT NULL,
            op TEXT NOT NULL CHECK(op IN ('upsert', 'delete')),
            blob_id TEXT REFERENCES blobs(blob_id),
            meta_json TEXT,
            updated_at_ms INTEGER NOT NULL,
            PRIMARY KEY (branch, path)
        );

        CREATE TABLE IF NOT EXISTS chunk_index (
            blob_id TEXT NOT NULL REFERENCES blobs(blob_id) ON DELETE CASCADE,
            chunk_seq INTEGER NOT NULL,
            title TEXT NOT NULL,
            aliases TEXT NOT NULL,
            tags TEXT NOT NULL,
            heading_path TEXT NOT NULL,
            chunk_text TEXT NOT NULL,
            raw_snippet TEXT NOT NULL,
            start_line INTEGER NOT NULL,
            end_line INTEGER NOT NULL,
            embedding BLOB,
            PRIMARY KEY (blob_id, chunk_seq)
        );

        CREATE VIRTUAL TABLE IF NOT EXISTS fts_chunks USING fts5(
            blob_id UNINDEXED,
            chunk_seq UNINDEXED,
            title,
            aliases,
            tags,
            heading_path,
            chunk_text,
            tokenize = 'unicode61'
        );

        CREATE TABLE IF NOT EXISTS blob_links (
            source_blob_id TEXT NOT NULL REFERENCES blobs(blob_id) ON DELETE CASCADE,
            link_kind TEXT NOT NULL CHECK(link_kind IN ('wikilink', 'tag', 'entity_rel')),
            source_entity TEXT NOT NULL,
            relation TEXT NOT NULL,
            target_ref TEXT NOT NULL,
            PRIMARY KEY (source_blob_id, link_kind, source_entity, relation, target_ref)
        );

        CREATE TABLE IF NOT EXISTS blob_index_model (
            blob_id TEXT PRIMARY KEY REFERENCES blobs(blob_id) ON DELETE CASCADE,
            model TEXT NOT NULL
        );

        CREATE INDEX IF NOT EXISTS idx_trees_tree_id ON trees(tree_id);
        CREATE INDEX IF NOT EXISTS idx_workspace_entries_branch ON workspace_entries(branch);
        CREATE INDEX IF NOT EXISTS idx_blob_links_target ON blob_links(target_ref);
        CREATE INDEX IF NOT EXISTS idx_blob_links_source ON blob_links(source_blob_id);
        "#,
    )
    .map_err(|e| MemError::Storage(format!("failed to initialize sqlite schema: {e}")))?;

    Ok(())
}
