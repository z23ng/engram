use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use axum::routing::{any, get, post};
use axum::{Json, Router};
use mem_core::{MemoryNode, Revision, VirtualPath};
use mem_search::MemSearchEngine;
use mem_vcs::VersionControlEngine;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::{authorize, Action, ApiKey, ControlPlane, ServerError};

pub(crate) type Shared = Arc<ControlPlane>;

/// REST routes under `/api/v1`, the MCP endpoint, and the WebDAV vault endpoint.
pub fn router(cp: Shared) -> Router {
    Router::new()
        .route("/api/v1/projects/{project}/read", get(read))
        .route("/api/v1/projects/{project}/write", post(write))
        .route("/p/{project}/mcp", post(crate::mcp::mcp))
        .route("/p/{project}/dav/{branch}", any(crate::dav::dav))
        .route("/p/{project}/dav/{branch}/", any(crate::dav::dav))
        .route("/p/{project}/dav/{branch}/{*rest}", any(crate::dav::dav))
        .merge(crate::api::routes())
        .merge(crate::browse::routes())
        .merge(crate::changes::routes())
        .fallback(crate::web::fallback)
        .with_state(cp)
}

#[derive(Deserialize)]
struct ReadParams {
    path: String,
    branch: Option<String>,
}

#[derive(Deserialize)]
struct WriteBody {
    path: String,
    content: String,
    branch: Option<String>,
    commit_message: Option<String>,
}

async fn read(
    State(cp): State<Shared>,
    Path(project): Path<String>,
    headers: HeaderMap,
    Query(q): Query<ReadParams>,
) -> Result<Json<Value>, ServerError> {
    guarded(cp, headers, project, Action::Read, move |_, _, engine| {
        let path = VirtualPath::new(&q.path)?;
        let rev = Revision::BranchWorkspace(q.branch.unwrap_or_else(|| "main".into()));
        let node = engine
            .vcs()
            .read(&rev, &path)?
            .ok_or_else(|| ServerError::NotFound(format!("'{}' not found", path.as_str())))?;
        Ok(Json(json!({
            "path": path.as_str(),
            "blob_id": node.blob_id.map(|b| b.to_hex()),
            "content": node.content_str()?,
        })))
    })
    .await
}

/// Stages the node; commits with the key name as author when `commit_message` is given.
async fn write(
    State(cp): State<Shared>,
    Path(project): Path<String>,
    headers: HeaderMap,
    Json(body): Json<WriteBody>,
) -> Result<Json<Value>, ServerError> {
    let scope = project.clone();
    guarded(cp, headers, scope, Action::Write, move |cp, key, engine| {
        let path = VirtualPath::new(&body.path)?;
        let branch = body.branch.unwrap_or_else(|| "main".into());
        let node = MemoryNode::new(path.clone(), body.content.into_bytes());
        let blob_id = engine.vcs().write_node(&branch, node)?;
        engine.index_blob_if_needed(&path, &blob_id)?;
        let commit_id = match body.commit_message {
            Some(msg) => Some(engine.vcs().commit(&branch, &key.name, &msg)?.to_hex()),
            None => None,
        };
        cp.record_audit(key, &project, "rest.write", commit_id.as_deref())?;
        Ok(Json(json!({
            "path": path.as_str(),
            "branch": branch,
            "blob_id": blob_id.to_hex(),
            "commit_id": commit_id,
        })))
    })
    .await
}

/// Verifies the bearer key, authorizes `action` on `project`, then runs `f` off the async runtime.
pub(crate) async fn guarded<T, F>(
    cp: Shared,
    headers: HeaderMap,
    project: String,
    action: Action,
    f: F,
) -> Result<T, ServerError>
where
    T: Send + 'static,
    F: FnOnce(&ControlPlane, &ApiKey, MemSearchEngine) -> Result<T, ServerError> + Send + 'static,
{
    keyed(cp, headers, move |cp, key| {
        authorize(key, &project, action)?;
        f(cp, key, cp.engine(&project)?)
    })
    .await
}

/// Verifies the bearer key, then runs `f` off the async runtime.
pub(crate) async fn keyed<T, F>(cp: Shared, headers: HeaderMap, f: F) -> Result<T, ServerError>
where
    T: Send + 'static,
    F: FnOnce(&ControlPlane, &ApiKey) -> Result<T, ServerError> + Send + 'static,
{
    tokio::task::spawn_blocking(move || {
        let token = headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .ok_or(ServerError::Unauthorized)?;
        let key = cp.verify(token.trim())?;
        f(&cp, &key)
    })
    .await
    .map_err(|e| ServerError::Internal(format!("blocking task failed: {e}")))?
}
