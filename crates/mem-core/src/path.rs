//! Virtual filesystem path validation and memory tier classification.

use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::{MemError, Result};

/// Top-level cognitive memory namespace tiers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MemoryTier {
    /// Pinned high-density system rules, preferences, and active goals (`/core/...`).
    Core,
    /// Append-oriented timeline and trajectory logs (`/episodic/...`).
    Episodic,
    /// Structured knowledge cards, entity notes, and decisions (`/semantic/...`).
    Semantic,
    /// Playbooks, SOPs, and debugging recipes (`/procedural/...`).
    Procedural,
}

impl MemoryTier {
    /// Returns the lowercase virtual directory segment for this tier.
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Core => "core",
            Self::Episodic => "episodic",
            Self::Semantic => "semantic",
            Self::Procedural => "procedural",
        }
    }

    /// Parses a top-level path segment into a [`MemoryTier`].
    pub fn from_segment(s: &str) -> Option<Self> {
        match s {
            "core" => Some(Self::Core),
            "episodic" => Some(Self::Episodic),
            "semantic" => Some(Self::Semantic),
            "procedural" => Some(Self::Procedural),
            _ => None,
        }
    }
}

impl fmt::Display for MemoryTier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Normalized, traversal-safe virtual memory path rooted under a valid [`MemoryTier`].
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct VirtualPath(String);

impl VirtualPath {
    /// Normalizes and validates a raw virtual path string.
    ///
    /// Rules:
    /// - Must start with `/`.
    /// - Rejects backslashes (`\`), NUL bytes (`\0`), and control characters.
    /// - Collapses consecutive slashes (`//`) and trims trailing slashes.
    /// - Rejects `.` and `..` traversal segments.
    /// - Requires a valid [`MemoryTier`] as the root segment.
    pub fn new(raw: impl AsRef<str>) -> Result<Self> {
        let raw = raw.as_ref();
        if !raw.starts_with('/') {
            return Err(MemError::InvalidPath(format!(
                "path must start with '/': {raw}"
            )));
        }
        if raw
            .chars()
            .any(|c| c == '\\' || c == '\0' || c.is_control())
        {
            return Err(MemError::InvalidPath(format!(
                "path contains disallowed control or backslash characters: {raw}"
            )));
        }

        let mut segments = Vec::new();
        for seg in raw.split('/') {
            if seg.is_empty() {
                continue;
            }
            if seg == "." || seg == ".." {
                return Err(MemError::InvalidPath(format!(
                    "path traversal segment '{seg}' is not allowed: {raw}"
                )));
            }
            segments.push(seg);
        }

        let Some(first) = segments.first() else {
            return Err(MemError::InvalidPath(
                "root '/' is not a valid memory tier path".to_string(),
            ));
        };

        if MemoryTier::from_segment(first).is_none() {
            return Err(MemError::InvalidPath(format!(
                "unknown memory tier '/{first}'; expected /core, /episodic, /semantic, or /procedural"
            )));
        }

        Ok(Self(format!("/{}", segments.join("/"))))
    }

    /// Validates that this path points to a file inside a tier rather than a bare tier root.
    pub fn validate_file_path(&self) -> Result<()> {
        if self.segments().len() < 2 {
            return Err(MemError::InvalidPath(format!(
                "cannot write a memory node directly at tier root '{}'; provide a file name",
                self.0
            )));
        }
        Ok(())
    }

    /// Returns the normalized path string.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Returns the [`MemoryTier`] implied by the root segment.
    pub fn tier(&self) -> MemoryTier {
        let first = self
            .segments()
            .into_iter()
            .next()
            .expect("VirtualPath always has at least one segment");
        MemoryTier::from_segment(first).expect("VirtualPath root segment is always a valid tier")
    }

    /// Splits the normalized path into non-empty segments.
    pub fn segments(&self) -> Vec<&str> {
        self.0.trim_start_matches('/').split('/').collect()
    }

    /// Returns the final segment of the path.
    pub fn file_name(&self) -> Option<&str> {
        self.segments().last().copied()
    }

    /// Returns the parent directory path, or `None` if already at a tier root (e.g. `/core`).
    pub fn parent(&self) -> Option<VirtualPath> {
        let segs = self.segments();
        if segs.len() <= 1 {
            return None;
        }
        let parent_str = format!("/{}", segs[..segs.len() - 1].join("/"));
        Some(Self(parent_str))
    }

    /// Checks whether `self` equals `prefix` or is nested under `prefix` on a segment boundary.
    pub fn starts_with(&self, prefix: &VirtualPath) -> bool {
        if self == prefix {
            return true;
        }
        self.0.len() > prefix.0.len()
            && self.0.starts_with(&prefix.0)
            && self.0.as_bytes()[prefix.0.len()] == b'/'
    }

    /// Returns `true` if `self` is a strict parent/ancestor directory of `other`.
    pub fn is_strict_ancestor_of(&self, other: &VirtualPath) -> bool {
        other != self && other.starts_with(self)
    }
}

impl fmt::Display for VirtualPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for VirtualPath {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl TryFrom<&str> for VirtualPath {
    type Error = MemError;

    fn try_from(value: &str) -> Result<Self> {
        Self::new(value)
    }
}

impl TryFrom<String> for VirtualPath {
    type Error = MemError;

    fn try_from(value: String) -> Result<Self> {
        Self::new(value)
    }
}

impl Serialize for VirtualPath {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for VirtualPath {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = String::deserialize(deserializer)?;
        Self::new(raw).map_err(serde::de::Error::custom)
    }
}
