use std::fmt;

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use mem_core::MemError;
use serde_json::json;

/// Error type shared by the control plane and the HTTP layer.
#[derive(Debug)]
pub enum ServerError {
    Unauthorized,
    Forbidden(String),
    NotFound(String),
    BadRequest(String),
    Internal(String),
}

impl fmt::Display for ServerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unauthorized => f.write_str("missing, unknown, revoked, or expired API key"),
            Self::Forbidden(m) | Self::NotFound(m) | Self::BadRequest(m) | Self::Internal(m) => {
                f.write_str(m)
            }
        }
    }
}

impl std::error::Error for ServerError {}

impl From<rusqlite::Error> for ServerError {
    fn from(err: rusqlite::Error) -> Self {
        Self::Internal(format!("control plane sqlite error: {err}"))
    }
}

impl From<MemError> for ServerError {
    fn from(err: MemError) -> Self {
        match err {
            MemError::Storage(_) | MemError::Serialization(_) | MemError::Compression(_) => {
                Self::Internal(err.to_string())
            }
            MemError::BranchNotFound(_)
            | MemError::CommitNotFound(_)
            | MemError::TreeNotFound(_)
            | MemError::BlobNotFound(_) => Self::NotFound(err.to_string()),
            _ => Self::BadRequest(err.to_string()),
        }
    }
}

impl IntoResponse for ServerError {
    fn into_response(self) -> Response {
        let status = match self {
            Self::Unauthorized => StatusCode::UNAUTHORIZED,
            Self::Forbidden(_) => StatusCode::FORBIDDEN,
            Self::NotFound(_) => StatusCode::NOT_FOUND,
            Self::BadRequest(_) => StatusCode::BAD_REQUEST,
            Self::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        };
        (status, Json(json!({ "error": self.to_string() }))).into_response()
    }
}
