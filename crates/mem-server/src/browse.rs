//! Read-only REST endpoints for the web client: branches, tree, log, status, diff, search,
//! index stats, raw.

use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, HeaderValue};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use mem_core::{CommitId, Revision, VirtualPath};
use mem_search::SearchRequest;
use mem_store::WorkspaceOp;
use mem_vcs::VersionControlEngine;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::http::{guarded, Shared};
use crate::{Action, ServerError};

pub(crate) fn routes() -> Router<Shared> {
    Router::new()
        .route("/api/v1/projects/{project}/branches", get(branches))
        .route("/api/v1/projects/{project}/tree", get(tree))
        .route("/api/v1/projects/{project}/log", get(log))
        .route("/api/v1/projects/{project}/status", get(status))
        .route("/api/v1/projects/{project}/diff", get(diff))
        .route("/api/v1/projects/{project}/search", get(search))
        .route("/api/v1/projects/{project}/graph", get(graph))
        .route("/api/v1/projects/{project}/index", get(index_info))
        .route("/api/v1/projects/{project}/raw", get(raw))
}

type JsonResult = Result<Json<Value>, ServerError>;

/// Query parameters shared by the browse endpoints; `branch` defaults to `main`.
#[derive(Deserialize)]
struct Q {
    branch: Option<String>,
    prefix: Option<String>,
    path: Option<String>,
    limit: Option<usize>,
    q: Option<String>,
    base: Option<String>,
    target: Option<String>,
}

impl Q {
    fn branch(&self) -> String {
        self.branch.clone().unwrap_or_else(|| "main".into())
    }

    fn required<'a>(&self, name: &str, value: &'a Option<String>) -> Result<&'a str, ServerError> {
        value
            .as_deref()
            .ok_or_else(|| ServerError::BadRequest(format!("query parameter '{name}' is required")))
    }
}

/// `workspace:<branch>` = staged view, 64-hex = commit, anything else = branch head.
fn revision(raw: &str) -> Revision {
    if let Some(branch) = raw.strip_prefix("workspace:") {
        Revision::BranchWorkspace(branch.into())
    } else if let Ok(commit) = CommitId::from_hex(raw) {
        Revision::Commit(commit)
    } else {
        Revision::BranchHead(raw.into())
    }
}

async fn branches(
    State(cp): State<Shared>,
    Path(p): Path<String>,
    headers: HeaderMap,
) -> JsonResult {
    guarded(cp, headers, p, Action::Read, |_, _, engine| {
        let store = engine.vcs().store();
        let mut refs = store.list_refs()?;
        refs.sort_by(|a, b| a.0.cmp(&b.0));
        let mut out = Vec::with_capacity(refs.len());
        for (name, head) in refs {
            let changes = store.workspace_list(&name)?.len();
            out.push(json!({"name": name, "head": head, "changes": changes}));
        }
        Ok(Json(json!({"branches": out})))
    })
    .await
}

async fn tree(
    State(cp): State<Shared>,
    Path(p): Path<String>,
    headers: HeaderMap,
    Query(q): Query<Q>,
) -> JsonResult {
    guarded(cp, headers, p, Action::Read, move |_, _, engine| {
        let prefix = q.prefix.as_deref().map(VirtualPath::new).transpose()?;
        let rev = Revision::BranchWorkspace(q.branch());
        let files: Vec<Value> = engine
            .vcs()
            .list(&rev, prefix.as_ref())?
            .into_iter()
            .map(|(path, blob_id, meta)| {
                json!({"path": path, "blob_id": blob_id, "content_type": meta.content_type,
                    "updated_at_ms": meta.updated_at_ms})
            })
            .collect();
        Ok(Json(json!({"files": files})))
    })
    .await
}

async fn log(
    State(cp): State<Shared>,
    Path(p): Path<String>,
    headers: HeaderMap,
    Query(q): Query<Q>,
) -> JsonResult {
    guarded(cp, headers, p, Action::Read, move |_, _, engine| {
        let commits = engine
            .vcs()
            .log(&q.branch(), q.limit.unwrap_or(50).min(500))?;
        Ok(Json(json!({"commits": commits})))
    })
    .await
}

async fn status(
    State(cp): State<Shared>,
    Path(p): Path<String>,
    headers: HeaderMap,
    Query(q): Query<Q>,
) -> JsonResult {
    guarded(cp, headers, p, Action::Read, move |_, _, engine| {
        let branch = q.branch();
        engine.vcs().head_commit(&branch)?;
        let changes: Vec<Value> = engine
            .vcs()
            .store()
            .workspace_list(&branch)?
            .into_iter()
            .map(|e| {
                let op = match e.op {
                    WorkspaceOp::Upsert => "upsert",
                    WorkspaceOp::Delete => "delete",
                };
                json!({"path": e.path, "op": op})
            })
            .collect();
        Ok(Json(json!({"changes": changes})))
    })
    .await
}

async fn diff(
    State(cp): State<Shared>,
    Path(p): Path<String>,
    headers: HeaderMap,
    Query(q): Query<Q>,
) -> JsonResult {
    guarded(cp, headers, p, Action::Read, move |_, _, engine| {
        let base = revision(q.required("base", &q.base)?);
        let target = revision(q.required("target", &q.target)?);
        let entries = engine.vcs().diff(&base, &target)?;
        Ok(Json(json!({"entries": entries})))
    })
    .await
}

async fn search(
    State(cp): State<Shared>,
    Path(p): Path<String>,
    headers: HeaderMap,
    Query(q): Query<Q>,
) -> JsonResult {
    guarded(cp, headers, p, Action::Read, move |_, _, engine| {
        let text = q.required("q", &q.q)?;
        let rev = Revision::BranchWorkspace(q.branch());
        let mut req = SearchRequest::new(rev, text).with_top_k(q.limit.unwrap_or(10).min(50));
        if let Some(prefix) = q.prefix.as_deref() {
            req = req.with_prefix(VirtualPath::new(prefix)?);
        }
        Ok(Json(json!({"hits": engine.search(&req)?})))
    })
    .await
}

async fn graph(
    State(cp): State<Shared>,
    Path(p): Path<String>,
    headers: HeaderMap,
    Query(q): Query<Q>,
) -> JsonResult {
    guarded(cp, headers, p, Action::Read, move |_, _, engine| {
        let rev = Revision::BranchWorkspace(q.branch());
        let g = engine.branch_graph(&rev)?;
        Ok(Json(json!(g)))
    })
    .await
}

/// Active embedding model and index coverage (counts are per indexed content version).
async fn index_info(
    State(cp): State<Shared>,
    Path(p): Path<String>,
    headers: HeaderMap,
) -> JsonResult {
    guarded(cp, headers, p, Action::Read, move |_, _, engine| {
        let (files, files_with_vectors, chunks) = engine.index_stats()?;
        Ok(Json(json!({
            "model": engine.embedding_model(),
            "files": files,
            "files_with_vectors": files_with_vectors,
            "chunks": chunks,
        })))
    })
    .await
}

/// Stored types come from MCP/ingest metadata too, so only inert types are served inline:
/// the 4 image types, and `text/*` as plain text. Everything else downloads as octet-stream.
fn served_type(stored: &str) -> (&'static str, bool) {
    match stored {
        "image/png" => ("image/png", true),
        "image/jpeg" => ("image/jpeg", true),
        "image/gif" => ("image/gif", true),
        "image/webp" => ("image/webp", true),
        t if t.starts_with("text/") => ("text/plain; charset=utf-8", true),
        _ => ("application/octet-stream", false),
    }
}

/// File bytes for the web client (images in the Markdown preview); never active content.
async fn raw(
    State(cp): State<Shared>,
    Path(p): Path<String>,
    headers: HeaderMap,
    Query(q): Query<Q>,
) -> Result<Response, ServerError> {
    guarded(cp, headers, p, Action::Read, move |_, _, engine| {
        let path = VirtualPath::new(q.required("path", &q.path)?)?;
        let node = engine
            .vcs()
            .read(&Revision::BranchWorkspace(q.branch()), &path)?
            .ok_or_else(|| ServerError::NotFound(format!("'{}' not found", path.as_str())))?;
        let (content_type, inline) = served_type(&node.metadata.content_type);
        let mut headers = HeaderMap::new();
        headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
        headers.insert(
            header::X_CONTENT_TYPE_OPTIONS,
            HeaderValue::from_static("nosniff"),
        );
        headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
        headers.insert(
            header::CONTENT_SECURITY_POLICY,
            HeaderValue::from_static("sandbox; default-src 'none'"),
        );
        if !inline {
            headers.insert(
                header::CONTENT_DISPOSITION,
                HeaderValue::from_static("attachment"),
            );
        }
        Ok((headers, node.content).into_response())
    })
    .await
}
