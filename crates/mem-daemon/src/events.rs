//! Real-time event broadcast payloads streamed over `/v1/events` SSE.

use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Structured mutation/consolidation event emitted by `mem-daemon`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MemoryEvent {
    /// Event kind (`node_written`, `node_deleted`, `committed`, `branch_created`,
    /// `branch_deleted`, `rewound`, `merged`, `ingested`, `consolidated`).
    pub event: String,
    /// Primary branch affected by the event.
    pub branch: String,
    /// Unix timestamp in milliseconds when the event was emitted.
    pub timestamp_ms: i64,
    /// Event-specific structured metadata.
    pub data: Value,
}

impl MemoryEvent {
    /// Creates a new [`MemoryEvent`] stamped with the current UTC time.
    pub fn new(event: impl Into<String>, branch: impl Into<String>, data: Value) -> Self {
        Self {
            event: event.into(),
            branch: branch.into(),
            timestamp_ms: Utc::now().timestamp_millis(),
            data,
        }
    }
}
