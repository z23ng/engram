//! Embedded single-page Web Console assets and handlers for `mem-daemon`.

use axum::http::header;
use axum::response::{Html, IntoResponse};

/// Self-contained single-page Web Console HTML served at `/` and `/ui`.
pub const INDEX_HTML: &str = include_str!("ui/index.html");

/// Serves the built-in Engram Web Console without requiring a browser login header upfront.
pub async fn index_html() -> impl IntoResponse {
    (
        [(header::CACHE_CONTROL, "no-cache, no-store, must-revalidate")],
        Html(INDEX_HTML),
    )
}
