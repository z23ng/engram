//! Memory node payloads and deterministic metadata definitions.

use chrono::Utc;
use serde::{Deserialize, Serialize};

use crate::{BlobId, Result, VirtualPath};

/// Structured metadata stored alongside a memory node in Merkle tree entries.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemoryMetadata {
    /// MIME type of the node payload (e.g., `"text/markdown"`, `"application/json"`).
    pub content_type: String,
    /// Relative importance score in `[0.0, 1.0]` used by ranking and core compiler.
    pub importance: f32,
    /// Searchable categorical tags attached to this memory node.
    pub tags: Vec<String>,
    /// Creation timestamp in Unix milliseconds.
    pub created_at_ms: i64,
    /// Last modification timestamp in Unix milliseconds.
    pub updated_at_ms: i64,
    /// Optional expiration duration in seconds.
    pub ttl_seconds: Option<u64>,
}

impl Default for MemoryMetadata {
    fn default() -> Self {
        let now = Utc::now().timestamp_millis();
        Self {
            content_type: "text/markdown".to_string(),
            importance: 0.5,
            tags: Vec::new(),
            created_at_ms: now,
            updated_at_ms: now,
            ttl_seconds: None,
        }
    }
}

impl MemoryMetadata {
    /// Serializes metadata into a canonical deterministic JSON string.
    ///
    /// Tags are sorted and deduplicated, and `-0.0` is normalized to `0.0` so
    /// identical logical metadata always hashes to the exact same Merkle entry.
    pub fn canonical_json(&self) -> Result<String> {
        let mut normalized_tags = self.tags.clone();
        normalized_tags.sort();
        normalized_tags.dedup();

        let normalized_importance = if self.importance == 0.0 {
            0.0
        } else {
            self.importance
        };

        let canonical = MemoryMetadata {
            content_type: self.content_type.clone(),
            importance: normalized_importance,
            tags: normalized_tags,
            created_at_ms: self.created_at_ms,
            updated_at_ms: self.updated_at_ms,
            ttl_seconds: self.ttl_seconds,
        };

        Ok(serde_json::to_string(&canonical)?)
    }

    /// Returns `true` when `ttl_seconds` is set and `reference_ms` is at or past expiration.
    pub fn is_expired_at(&self, reference_ms: i64) -> bool {
        self.ttl_seconds.is_some_and(|ttl| {
            let ttl_ms = i64::try_from(ttl)
                .unwrap_or(i64::MAX / 2)
                .saturating_mul(1000);
            reference_ms >= self.created_at_ms.saturating_add(ttl_ms)
        })
    }
}

/// Complete virtual memory file node read from or written to a branch workspace.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemoryNode {
    /// Normalized virtual path in the memory VFS.
    pub path: VirtualPath,
    /// Raw memory payload bytes.
    pub content: Vec<u8>,
    /// Associated structured metadata.
    pub metadata: MemoryMetadata,
    /// Optional content-addressed blob digest populated on reads or after writes.
    pub blob_id: Option<BlobId>,
}

impl MemoryNode {
    /// Creates a new memory node with default metadata for `path` and `content`.
    pub fn new(path: VirtualPath, content: impl Into<Vec<u8>>) -> Self {
        Self {
            path,
            content: content.into(),
            metadata: MemoryMetadata::default(),
            blob_id: None,
        }
    }

    /// Replaces the metadata of this node.
    pub fn with_metadata(mut self, metadata: MemoryMetadata) -> Self {
        self.metadata = metadata;
        self
    }

    /// Sets the tag list of this node.
    pub fn with_tags(mut self, tags: Vec<String>) -> Self {
        self.metadata.tags = tags;
        self
    }

    /// Sets the importance score of this node.
    pub fn with_importance(mut self, importance: f32) -> Self {
        self.metadata.importance = importance;
        self
    }

    /// Sets the optional TTL (in seconds) of this node.
    pub fn with_ttl_seconds(mut self, ttl_seconds: Option<u64>) -> Self {
        self.metadata.ttl_seconds = ttl_seconds;
        self
    }

    /// Decodes the raw content bytes as UTF-8 text.
    pub fn content_str(&self) -> Result<&str> {
        Ok(std::str::from_utf8(&self.content)?)
    }
}
