//! Embedded web client: files from `web/dist` (built by `npm --prefix web run build`).

use axum::http::{header, HeaderValue, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use rust_embed::Embed;

#[derive(Embed)]
#[folder = "../../web/dist"]
#[allow_missing = true]
struct Assets;

/// Same-origin only. Inline styles are needed by CodeMirror; `blob:` images come from `/raw`.
const CSP: &str = "default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; \
    img-src 'self' blob: data:; font-src 'self'; connect-src 'self'; object-src 'none'; \
    base-uri 'none'; form-action 'self'; frame-ancestors 'none'";

const NOT_BUILT: &str =
    "web client not built: run `npm --prefix web ci && npm --prefix web run build`, \
    then rebuild mem-server";

/// Router fallback: `/assets/*` files, else `index.html` for client routes.
/// `/api/` and `/p/` paths never get HTML, so API clients see a plain 404.
pub(crate) async fn fallback(uri: Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    let reserved = path.starts_with("api/") || path.starts_with("p/");
    if reserved || path.split('/').any(|s| s == "..") {
        return StatusCode::NOT_FOUND.into_response();
    }
    if path.starts_with("assets/") || path.contains('.') {
        return match Assets::get(path) {
            Some(file) => {
                let mime = HeaderValue::from_str(file.metadata.mimetype())
                    .unwrap_or(HeaderValue::from_static("application/octet-stream"));
                let cache = HeaderValue::from_static("public, max-age=31536000, immutable");
                let headers = [(header::CONTENT_TYPE, mime), (header::CACHE_CONTROL, cache)];
                (headers, file.data.into_owned()).into_response()
            }
            None => StatusCode::NOT_FOUND.into_response(),
        };
    }
    let Some(index) = Assets::get("index.html") else {
        return (StatusCode::SERVICE_UNAVAILABLE, NOT_BUILT).into_response();
    };
    let headers = [
        (header::CONTENT_TYPE, "text/html; charset=utf-8"),
        (header::CACHE_CONTROL, "no-cache"),
        (header::CONTENT_SECURITY_POLICY, CSP),
        (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        (header::REFERRER_POLICY, "no-referrer"),
    ];
    (headers, index.data.into_owned()).into_response()
}
