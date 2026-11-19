//! Content-addressed blob storage backed by BLAKE3 and zstd compression.

use chrono::Utc;
use mem_core::{BlobId, MemError, Result};
use rusqlite::{params, Connection, OptionalExtension};

const ZSTD_LEVEL: i32 = 3;

/// Stores raw payload bytes into the immutable `blobs` table using BLAKE3 + zstd.
pub fn put_blob_conn(conn: &Connection, raw_bytes: &[u8], content_type: &str) -> Result<BlobId> {
    let blob_id = BlobId::hash_bytes(raw_bytes);
    if has_blob_conn(conn, &blob_id)? {
        return Ok(blob_id);
    }

    let compressed = zstd::encode_all(raw_bytes, ZSTD_LEVEL)
        .map_err(|e| MemError::Compression(format!("zstd compress failed: {e}")))?;
    let raw_size = i64::try_from(raw_bytes.len())
        .map_err(|_| MemError::Storage("blob size exceeds i64::MAX".to_string()))?;
    let now_ms = Utc::now().timestamp_millis();

    conn.execute(
        "INSERT OR IGNORE INTO blobs (blob_id, content, raw_size, content_type, created_at_ms)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            blob_id.to_hex(),
            compressed,
            raw_size,
            content_type,
            now_ms
        ],
    )
    .map_err(|e| MemError::Storage(format!("failed to insert blob: {e}")))?;

    Ok(blob_id)
}

/// Reads and verifies a blob by its [`BlobId`], returning `(raw_bytes, content_type)`.
pub fn get_blob_conn(conn: &Connection, blob_id: &BlobId) -> Result<Option<(Vec<u8>, String)>> {
    let row: Option<(Vec<u8>, i64, String)> = conn
        .query_row(
            "SELECT content, raw_size, content_type FROM blobs WHERE blob_id = ?1",
            params![blob_id.to_hex()],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()
        .map_err(|e| MemError::Storage(format!("failed to query blob: {e}")))?;

    let Some((compressed, raw_size, content_type)) = row else {
        return Ok(None);
    };

    let raw = zstd::decode_all(compressed.as_slice())
        .map_err(|e| MemError::Compression(format!("zstd decompress failed: {e}")))?;

    if raw.len() as i64 != raw_size {
        return Err(MemError::Storage(format!(
            "corrupted blob {blob_id}: expected raw_size {raw_size}, got {}",
            raw.len()
        )));
    }

    let actual_id = BlobId::hash_bytes(&raw);
    if actual_id != *blob_id {
        return Err(MemError::Storage(format!(
            "corrupted blob {blob_id}: digest mismatch (actual {actual_id})"
        )));
    }

    Ok(Some((raw, content_type)))
}

/// Checks whether a blob with `blob_id` already exists in `blobs`.
pub fn has_blob_conn(conn: &Connection, blob_id: &BlobId) -> Result<bool> {
    let exists: Option<i64> = conn
        .query_row(
            "SELECT 1 FROM blobs WHERE blob_id = ?1",
            params![blob_id.to_hex()],
            |r| r.get(0),
        )
        .optional()
        .map_err(|e| MemError::Storage(format!("failed to check blob existence: {e}")))?;
    Ok(exists.is_some())
}

/// Returns the total number of distinct blobs stored in `blobs`.
pub fn blob_count_conn(conn: &Connection) -> Result<usize> {
    count_table(conn, "blobs")
}

/// Returns the total number of Merkle tree nodes stored in `tree_objects`.
pub fn tree_count_conn(conn: &Connection) -> Result<usize> {
    count_table(conn, "tree_objects")
}

/// Returns the total number of commits stored in `commits`.
pub fn commit_count_conn(conn: &Connection) -> Result<usize> {
    count_table(conn, "commits")
}

fn count_table(conn: &Connection, table: &str) -> Result<usize> {
    let sql = format!("SELECT COUNT(*) FROM {table}");
    let count: i64 = conn
        .query_row(&sql, [], |r| r.get(0))
        .map_err(|e| MemError::Storage(format!("failed to count {table}: {e}")))?;
    usize::try_from(count).map_err(|_| MemError::Storage("negative row count".to_string()))
}
