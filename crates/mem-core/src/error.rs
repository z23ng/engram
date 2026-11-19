//! Core error definitions for the AI Memory (`mem`) engine.

use std::str::Utf8Error;
use std::string::FromUtf8Error;

/// Unified error type across `mem-core`, `mem-store`, and `mem-vcs`.
#[derive(Debug, thiserror::Error)]
pub enum MemError {
    /// Virtual path failed normalization or tier validation.
    #[error("invalid virtual path: {0}")]
    InvalidPath(String),

    /// BLAKE3 hash string could not be parsed as a 32-byte digest.
    #[error("invalid hash digest: {0}")]
    InvalidHash(String),

    /// Target branch reference does not exist.
    #[error("branch not found: {0}")]
    BranchNotFound(String),

    /// Branch reference already exists when creating a new branch.
    #[error("branch already exists: {0}")]
    BranchAlreadyExists(String),

    /// Deleting the default `main` branch is disallowed.
    #[error("cannot delete default main branch")]
    CannotDeleteMainBranch,

    /// Commit ID does not exist in storage.
    #[error("commit not found: {0}")]
    CommitNotFound(String),

    /// Merkle tree ID does not exist in storage.
    #[error("tree not found: {0}")]
    TreeNotFound(String),

    /// Content blob ID does not exist in storage.
    #[error("blob not found: {0}")]
    BlobNotFound(String),

    /// File/directory hierarchy conflict on a virtual path.
    #[error("directory/file conflict at {path}: {detail}")]
    DirectoryFileConflict {
        /// Conflicting virtual path.
        path: String,
        /// Explanation of the conflict.
        detail: String,
    },

    /// Underlying SQLite or I/O storage failure.
    #[error("storage error: {0}")]
    Storage(String),

    /// JSON or canonical metadata serialization error.
    #[error("serialization error: {0}")]
    Serialization(String),

    /// zstd payload compression or decompression error.
    #[error("compression error: {0}")]
    Compression(String),

    /// Invalid UTF-8 content when decoding text memory payloads.
    #[error("utf-8 decoding error: {0}")]
    Utf8(String),
}

/// Convenience result type for AI Memory operations.
pub type Result<T> = std::result::Result<T, MemError>;

impl From<serde_json::Error> for MemError {
    fn from(err: serde_json::Error) -> Self {
        Self::Serialization(err.to_string())
    }
}

impl From<Utf8Error> for MemError {
    fn from(err: Utf8Error) -> Self {
        Self::Utf8(err.to_string())
    }
}

impl From<FromUtf8Error> for MemError {
    fn from(err: FromUtf8Error) -> Self {
        Self::Utf8(err.to_string())
    }
}
