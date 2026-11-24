//! WebDAV vault endpoint `/p/{project}/dav/{branch}/...` (Obsidian sync).

use std::collections::HashMap;
use std::sync::Arc;

use axum::body::{to_bytes, Body};
use axum::extract::{Path, Request, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use base64::Engine as _;
use dav_server::fakels::FakeLs;
use dav_server::DavHandler;

use crate::davfs::{BranchFs, MAX_FILE};
use crate::http::guarded;
use crate::{Action, ControlPlane, ServerError};

/// Maps a WebDAV method to its permission class; `None` means 405 (deny by default).
fn method_action(method: &str) -> Option<Action> {
    match method {
        "OPTIONS" | "PROPFIND" | "GET" | "HEAD" => Some(Action::Read),
        "PUT" | "DELETE" | "MKCOL" | "MOVE" | "COPY" | "PROPPATCH" | "LOCK" | "UNLOCK" => {
            Some(Action::Write)
        }
        _ => None,
    }
}

pub(crate) async fn dav(
    State(cp): State<Arc<ControlPlane>>,
    Path(params): Path<HashMap<String, String>>,
    req: Request,
) -> Response {
    match serve(cp, params, req).await {
        Ok(resp) => resp,
        Err(ServerError::Unauthorized) => (
            StatusCode::UNAUTHORIZED,
            [(header::WWW_AUTHENTICATE, "Basic realm=\"engram\"")],
        )
            .into_response(),
        Err(err) => err.into_response(),
    }
}

async fn serve(
    cp: Arc<ControlPlane>,
    params: HashMap<String, String>,
    req: Request,
) -> Result<Response, ServerError> {
    let Some(action) = method_action(req.method().as_str()) else {
        return Ok(StatusCode::METHOD_NOT_ALLOWED.into_response());
    };
    let project = params.get("project").cloned().unwrap_or_default();
    // dav-server rejects a decoded '/' inside a segment, so DAV URLs write '/' as '~'
    // (git also forbids '~' in ref names): /p/alpha/dav/draft~pm-inbox/ -> branch draft/pm-inbox.
    let segment = params.get("branch").cloned().unwrap_or_default();
    if segment.contains('/') {
        return Err(ServerError::BadRequest(
            "write '/' in the branch name as '~' in WebDAV URLs".into(),
        ));
    }
    let branch = segment.replace('~', "/");
    let headers = bearer_headers(req.headers());
    let head_branch = branch.clone();
    let (key, engine) = guarded(
        cp.clone(),
        headers,
        project.clone(),
        action,
        move |_, key, engine| {
            engine.vcs().head_commit(&head_branch)?;
            Ok((key.clone(), engine))
        },
    )
    .await?;

    let (parts, body) = req.into_parts();
    let Ok(bytes) = to_bytes(body, MAX_FILE).await else {
        return Ok(StatusCode::PAYLOAD_TOO_LARGE.into_response());
    };
    let req = Request::from_parts(parts, Body::from(bytes));
    let prefix = format!("/p/{project}/dav/{segment}");
    let fs = BranchFs {
        cp,
        engine,
        project,
        branch,
        key,
    };
    let handler = DavHandler::builder()
        .filesystem(Box::new(fs))
        .locksystem(FakeLs::new())
        .strip_prefix(prefix)
        .build_handler();
    Ok(handler.handle(req).await.map(Body::new))
}

/// Converts `Basic base64(user:key)` or `Bearer key` into a `Bearer key` header for `guarded`.
fn bearer_headers(headers: &HeaderMap) -> HeaderMap {
    let raw = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    let token = if let Some(encoded) = raw.strip_prefix("Basic ") {
        base64::engine::general_purpose::STANDARD
            .decode(encoded.trim())
            .ok()
            .and_then(|b| String::from_utf8(b).ok())
            .and_then(|s| s.split_once(':').map(|(_, pw)| pw.to_string()))
    } else {
        raw.strip_prefix("Bearer ").map(String::from)
    };
    let mut out = HeaderMap::new();
    if let Some(value) = token.and_then(|t| HeaderValue::from_str(&format!("Bearer {t}")).ok()) {
        out.insert(header::AUTHORIZATION, value);
    }
    out
}
