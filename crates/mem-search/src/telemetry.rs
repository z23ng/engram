//! Sidecar retrieval & access telemetry for closed-loop memory utility scoring.
//!
//! All telemetry lives in `memory_telemetry` keyed by `(branch, path)` and tracks `blob_id`
//! so counters reset cleanly whenever a node is overwritten, preserving CAS immutability and
//! strict branch isolation.

use std::collections::HashMap;

use chrono::Utc;
use mem_core::{BlobId, MemError, Result, VirtualPath};
use mem_store::SqliteStore;
use rusqlite::{params, Connection};

const TELEMETRY_UPSERT_SQL: &str = "
    INSERT INTO memory_telemetry (
        branch, path, blob_id, search_hits, read_hits, post_hit_commits, post_hit_rewinds, last_accessed_ms
    ) VALUES (?1, ?2, ?3, ?4, ?5, 0, 0, ?6)
    ON CONFLICT(branch, path) DO UPDATE SET
        search_hits = CASE
            WHEN memory_telemetry.blob_id = excluded.blob_id
                THEN memory_telemetry.search_hits + excluded.search_hits
            ELSE excluded.search_hits
        END,
        read_hits = CASE
            WHEN memory_telemetry.blob_id = excluded.blob_id
                THEN memory_telemetry.read_hits + excluded.read_hits
            ELSE excluded.read_hits
        END,
        post_hit_commits = CASE
            WHEN memory_telemetry.blob_id = excluded.blob_id
                THEN memory_telemetry.post_hit_commits
            ELSE 0
        END,
        post_hit_rewinds = CASE
            WHEN memory_telemetry.blob_id = excluded.blob_id
                THEN memory_telemetry.post_hit_rewinds
            ELSE 0
        END,
        blob_id = excluded.blob_id,
        last_accessed_ms = excluded.last_accessed_ms
";

/// Per-node access and outcome telemetry row scoped to `(branch, path)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TelemetryRow {
    pub branch: String,
    pub path: VirtualPath,
    pub blob_id: String,
    pub search_hits: i64,
    pub read_hits: i64,
    pub post_hit_commits: i64,
    pub post_hit_rewinds: i64,
    pub last_accessed_ms: i64,
}

/// Records search impression hits for `(path, blob_id)` pairs returned on `branch`.
pub fn record_search_hits(
    store: &SqliteStore,
    branch: &str,
    hits: &[(VirtualPath, BlobId)],
) -> Result<()> {
    if hits.is_empty() {
        return Ok(());
    }
    let now_ms = Utc::now().timestamp_millis();
    store.with_tx(|tx| {
        let mut stmt = tx.prepare_cached(TELEMETRY_UPSERT_SQL).map_err(|e| {
            MemError::Storage(format!("failed to prepare search telemetry upsert: {e}"))
        })?;
        for (path, blob_id) in hits {
            stmt.execute(params![
                branch,
                path.as_str(),
                blob_id.to_hex(),
                1i64,
                0i64,
                now_ms
            ])
            .map_err(|e| MemError::Storage(format!("failed to record search telemetry: {e}")))?;
        }
        Ok(())
    })
}

/// Records a direct `mem_read` hit for `(path, blob_id)` on `branch`.
pub fn record_node_read(
    store: &SqliteStore,
    branch: &str,
    path: &VirtualPath,
    blob_id: &BlobId,
) -> Result<()> {
    let now_ms = Utc::now().timestamp_millis();
    store.with_conn(|conn| {
        let mut stmt = conn.prepare_cached(TELEMETRY_UPSERT_SQL).map_err(|e| {
            MemError::Storage(format!("failed to prepare read telemetry upsert: {e}"))
        })?;
        stmt.execute(params![
            branch,
            path.as_str(),
            blob_id.to_hex(),
            0i64,
            1i64,
            now_ms
        ])
        .map_err(|e| MemError::Storage(format!("failed to record read telemetry: {e}")))?;
        Ok(())
    })
}

/// Loads all telemetry rows recorded for `branch`.
pub fn load_branch_telemetry(
    conn: &Connection,
    branch: &str,
) -> Result<HashMap<VirtualPath, TelemetryRow>> {
    let mut stmt = conn
        .prepare_cached(
            "SELECT path, blob_id, search_hits, read_hits, post_hit_commits, post_hit_rewinds, last_accessed_ms
             FROM memory_telemetry WHERE branch = ?1",
        )
        .map_err(|e| MemError::Storage(format!("failed to prepare telemetry query: {e}")))?;

    let rows = stmt
        .query_map(params![branch], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, i64>(3)?,
                r.get::<_, i64>(4)?,
                r.get::<_, i64>(5)?,
                r.get::<_, i64>(6)?,
            ))
        })
        .map_err(|e| MemError::Storage(format!("failed to query branch telemetry: {e}")))?;

    let mut out = HashMap::new();
    for row in rows {
        let (
            path_str,
            blob_id,
            search_hits,
            read_hits,
            post_hit_commits,
            post_hit_rewinds,
            last_accessed_ms,
        ) = row.map_err(|e| MemError::Storage(format!("failed to decode telemetry row: {e}")))?;
        if let Ok(path) = VirtualPath::new(path_str) {
            out.insert(
                path.clone(),
                TelemetryRow {
                    branch: branch.to_string(),
                    path,
                    blob_id,
                    search_hits,
                    read_hits,
                    post_hit_commits,
                    post_hit_rewinds,
                    last_accessed_ms,
                },
            );
        }
    }
    Ok(out)
}

/// Computes a bounded ranking multiplier `[0.85, 1.15]` for `active_blob` based on utility signals.
/// Returns `1.0` when `row.blob_id != active_blob` or when no read/outcome feedback exists yet.
pub fn utility_multiplier(row: &TelemetryRow, active_blob: &BlobId) -> f32 {
    if row.blob_id != active_blob.to_hex() {
        return 1.0;
    }
    let pos =
        (row.read_hits.max(0) as f32 * 0.5 + row.post_hit_commits.max(0) as f32 * 1.5).min(10.0);
    let neg = (row.post_hit_rewinds.max(0) as f32 * 2.0).min(10.0);
    (1.0 + 0.015 * pos - 0.02 * neg).clamp(0.85, 1.15)
}
