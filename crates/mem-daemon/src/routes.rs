//! Axum HTTP REST, SSE event streaming, and HTTP/SSE MCP routes for `mem-daemon`.

use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use futures_util::stream::{self, Stream};
use mem_consolidate::{ConsolidationEngine, ConsolidationMergeStatus, ConsolidationRequest};
use mem_core::{
    CommitId, MemError, MemoryMetadata, MemoryNode, MemoryTier, MergeOutcome, MergeStrategy,
    Revision, VirtualPath,
};
use mem_search::{IngestRequest, IngestRole, SearchRequest};
use mem_vcs::VersionControlEngine;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::{mpsc, RwLock};
use tokio_stream::wrappers::{BroadcastStream, ReceiverStream};
use tokio_stream::StreamExt;
use tower_http::cors::CorsLayer;
use uuid::Uuid;

use crate::mcp_rpc::handle_jsonrpc_request;
use crate::state::DaemonState;

/// Structured HTTP API error wrapper mapping [`MemError`] variants cleanly to status codes.
#[derive(Debug)]
pub struct ApiError {
    pub status: StatusCode,
    pub code: &'static str,
    pub message: String,
}

impl ApiError {
    fn unauthorized(msg: impl Into<String>) -> Self {
        Self {
            status: StatusCode::UNAUTHORIZED,
            code: "unauthorized",
            message: msg.into(),
        }
    }

    fn bad_request(msg: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            code: "bad_request",
            message: msg.into(),
        }
    }

    fn not_found(msg: impl Into<String>) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            code: "not_found",
            message: msg.into(),
        }
    }

    fn internal(msg: impl Into<String>) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            code: "internal_error",
            message: msg.into(),
        }
    }
}

impl From<MemError> for ApiError {
    fn from(err: MemError) -> Self {
        let (status, code) = match &err {
            MemError::BranchNotFound(_)
            | MemError::CommitNotFound(_)
            | MemError::BlobNotFound(_)
            | MemError::TreeNotFound(_) => (StatusCode::NOT_FOUND, "not_found"),
            MemError::BranchAlreadyExists(_) => (StatusCode::CONFLICT, "conflict"),
            MemError::InvalidPath(_)
            | MemError::InvalidHash(_)
            | MemError::DirectoryFileConflict { .. }
            | MemError::CannotDeleteMainBranch
            | MemError::Serialization(_)
            | MemError::Utf8(_) => (StatusCode::BAD_REQUEST, "bad_request"),
            _ => (StatusCode::INTERNAL_SERVER_ERROR, "storage_error"),
        };
        Self {
            status,
            code,
            message: err.to_string(),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(json!({
                "error": self.message,
                "code": self.code,
            })),
        )
            .into_response()
    }
}

type ApiResult<T> = std::result::Result<T, ApiError>;

/// Constructs the complete Axum [`Router`] with Web UI, REST, SSE, and MCP endpoints.
pub fn build_router(state: DaemonState) -> Router {
    Router::new()
        .route("/", get(crate::ui::index_html))
        .route("/ui", get(crate::ui::index_html))
        .route("/health", get(health))
        .route("/v1/status", get(get_status))
        .route("/v1/nodes", get(list_nodes))
        .route(
            "/v1/node",
            get(read_node).put(write_node).delete(delete_node),
        )
        .route("/v1/commits", get(list_commits).post(commit_branch))
        .route(
            "/v1/branches",
            get(list_branches)
                .post(create_branch)
                .delete(delete_branch_query),
        )
        .route("/v1/branches/{*name}", delete(delete_branch_path))
        .route("/v1/rewind", post(rewind_branch))
        .route("/v1/diff", get(diff_revisions))
        .route("/v1/merge", post(merge_branches))
        .route("/v1/gc", post(run_gc))
        .route("/v1/search", post(search_memories))
        .route("/v1/graph", get(get_branch_graph))
        .route("/v1/compile", post(compile_core_route))
        .route("/v1/ingest", post(ingest_note))
        .route("/v1/consolidate", post(consolidate_branch))
        .route("/v1/events", get(stream_events))
        .route("/mcp", post(mcp_http_post))
        .route("/mcp/sse", get(mcp_sse_connect))
        .route("/mcp/messages", post(mcp_sse_message))
        .layer(CorsLayer::permissive())
        .with_state(state)
}

fn verify_auth(
    state: &DaemonState,
    headers: &HeaderMap,
    query_token: Option<&str>,
) -> ApiResult<()> {
    let Some(expected) = state.auth_token.as_deref() else {
        return Ok(());
    };
    if let Some(auth_val) = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
    {
        if let Some(bearer) = auth_val
            .strip_prefix("Bearer ")
            .or_else(|| auth_val.strip_prefix("bearer "))
        {
            if bearer.trim() == expected {
                return Ok(());
            }
        }
    }
    if let Some(tok) = query_token {
        if tok == expected {
            return Ok(());
        }
    }
    Err(ApiError::unauthorized("missing or invalid bearer token"))
}

async fn run_blocking<F, T>(f: F) -> ApiResult<T>
where
    F: FnOnce() -> mem_core::Result<T> + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| ApiError::internal(format!("blocking task panicked: {e}")))?
        .map_err(ApiError::from)
}

async fn health(State(state): State<DaemonState>) -> Json<Value> {
    Json(json!({
        "status": "ok",
        "service": "mem-daemon",
        "version": env!("CARGO_PKG_VERSION"),
        "db_path": state.db_path.display().to_string(),
        "auth_required": state.auth_token.is_some(),
        "started_at_ms": state.started_at_ms,
    }))
}

#[derive(Deserialize)]
struct StatusQuery {
    branch: Option<String>,
    token: Option<String>,
}

async fn get_status(
    State(state): State<DaemonState>,
    headers: HeaderMap,
    Query(q): Query<StatusQuery>,
) -> ApiResult<Json<Value>> {
    verify_auth(&state, &headers, q.token.as_deref())?;
    let branch = q.branch.unwrap_or_else(|| "main".to_string());
    let vcs = state.vcs.clone();
    let emb_cfg = state.emb_cfg.clone();
    let llm_cfg = state.llm_cfg.clone();
    let db_path = state.db_path.display().to_string();

    let payload = run_blocking(move || {
        let head = vcs.head_commit(&branch)?;
        let ws = vcs.store().workspace_list(&branch)?;
        let active = vcs.list(&Revision::BranchWorkspace(branch.clone()), None)?;
        let refs = vcs.store().list_refs()?;
        let mut tier_counts: HashMap<&'static str, usize> = HashMap::new();
        for (p, _, _) in &active {
            *tier_counts.entry(p.tier().as_str()).or_insert(0) += 1;
        }
        Ok(json!({
            "db_path": db_path,
            "branch": branch,
            "head_commit": head.to_hex(),
            "branches": refs.len(),
            "staged_changes": ws.len(),
            "active_nodes": active.len(),
            "tiers": tier_counts,
            "storage": {
                "blobs": vcs.store().blob_count()?,
                "trees": vcs.store().tree_count()?,
                "commits": vcs.store().commit_count()?,
            },
            "embedding_provider": emb_cfg,
            "consolidate_llm": llm_cfg,
        }))
    })
    .await?;

    Ok(Json(payload))
}

#[derive(Deserialize)]
struct ListNodesQuery {
    branch: Option<String>,
    prefix: Option<String>,
    token: Option<String>,
}

async fn list_nodes(
    State(state): State<DaemonState>,
    headers: HeaderMap,
    Query(q): Query<ListNodesQuery>,
) -> ApiResult<Json<Value>> {
    verify_auth(&state, &headers, q.token.as_deref())?;
    let branch = q.branch.unwrap_or_else(|| "main".to_string());
    let prefix = q
        .prefix
        .as_deref()
        .filter(|s| !s.trim().is_empty())
        .map(VirtualPath::new)
        .transpose()?;
    let vcs = state.vcs.clone();

    let items = run_blocking(move || {
        let entries = vcs.list(&Revision::BranchWorkspace(branch), prefix.as_ref())?;
        let mapped: Vec<Value> = entries
            .into_iter()
            .map(|(p, b, m)| {
                json!({
                    "path": p.as_str(),
                    "blob_id": b.to_hex(),
                    "importance": m.importance,
                    "tags": m.tags,
                    "updated_at_ms": m.updated_at_ms,
                })
            })
            .collect();
        Ok(json!(mapped))
    })
    .await?;

    Ok(Json(items))
}

#[derive(Deserialize)]
struct ReadNodeQuery {
    path: String,
    branch: Option<String>,
    commit_id: Option<String>,
    token: Option<String>,
}

async fn read_node(
    State(state): State<DaemonState>,
    headers: HeaderMap,
    Query(q): Query<ReadNodeQuery>,
) -> ApiResult<Json<Value>> {
    verify_auth(&state, &headers, q.token.as_deref())?;
    let vpath = VirtualPath::new(&q.path)?;
    let rev = match q.commit_id.as_deref().filter(|s| !s.trim().is_empty()) {
        Some(cid) => Revision::Commit(CommitId::from_hex(cid)?),
        None => Revision::BranchWorkspace(q.branch.unwrap_or_else(|| "main".to_string())),
    };
    let vcs = state.vcs.clone();

    let node_opt = run_blocking(move || vcs.read(&rev, &vpath)).await?;
    let Some(node) = node_opt else {
        return Err(ApiError::not_found(format!(
            "memory node not found at {}",
            q.path
        )));
    };
    let content = node.content_str()?.to_string();
    Ok(Json(json!({
        "path": node.path.as_str(),
        "blob_id": node.blob_id.map(|b| b.to_hex()),
        "metadata": node.metadata,
        "content": content,
    })))
}

#[derive(Deserialize)]
struct WriteNodeBody {
    path: String,
    content: String,
    #[serde(default = "default_main")]
    branch: String,
    tags: Option<Vec<String>>,
    importance: Option<f32>,
    content_type: Option<String>,
    ttl_seconds: Option<u64>,
}

async fn write_node(
    State(state): State<DaemonState>,
    headers: HeaderMap,
    Json(body): Json<WriteNodeBody>,
) -> ApiResult<Json<Value>> {
    verify_auth(&state, &headers, None)?;
    let vpath = VirtualPath::new(&body.path)?;
    vpath.validate_file_path()?;

    let mut meta = MemoryMetadata::default();
    if let Some(ct) = body.content_type {
        meta.content_type = ct;
    }
    if let Some(imp) = body.importance {
        meta.importance = imp.clamp(0.0, 1.0);
    }
    if let Some(tags) = body.tags {
        meta.tags = tags;
    }
    meta.ttl_seconds = body.ttl_seconds;

    let branch = body.branch;
    let content_bytes = body.content.into_bytes();
    let search = state.search.clone();
    let vpath_clone = vpath.clone();
    let branch_clone = branch.clone();

    let blob_id = run_blocking(move || {
        let node = MemoryNode::new(vpath_clone.clone(), content_bytes).with_metadata(meta);
        let blob_id = search.vcs().write_node(&branch_clone, node)?;
        search.index_blob_if_needed(&vpath_clone, &blob_id)?;
        Ok(blob_id)
    })
    .await?;

    let payload = json!({
        "status": "staged",
        "branch": branch,
        "path": vpath.as_str(),
        "blob_id": blob_id.to_hex(),
    });
    state.emit("node_written", &branch, payload.clone());
    Ok(Json(payload))
}

#[derive(Deserialize)]
struct DeleteNodeQuery {
    path: String,
    branch: Option<String>,
    token: Option<String>,
}

async fn delete_node(
    State(state): State<DaemonState>,
    headers: HeaderMap,
    Query(q): Query<DeleteNodeQuery>,
) -> ApiResult<Json<Value>> {
    verify_auth(&state, &headers, q.token.as_deref())?;
    let vpath = VirtualPath::new(&q.path)?;
    let branch = q.branch.unwrap_or_else(|| "main".to_string());
    let vcs = state.vcs.clone();
    let branch_clone = branch.clone();
    let vpath_clone = vpath.clone();

    let removed = run_blocking(move || vcs.delete_path(&branch_clone, &vpath_clone)).await?;
    let payload = json!({
        "branch": branch,
        "path": vpath.as_str(),
        "removed_count": removed,
    });
    state.emit("node_deleted", &branch, payload.clone());
    Ok(Json(payload))
}

#[derive(Deserialize)]
struct CommitBody {
    message: String,
    #[serde(default = "default_main")]
    branch: String,
    author: Option<String>,
}

async fn commit_branch(
    State(state): State<DaemonState>,
    headers: HeaderMap,
    Json(body): Json<CommitBody>,
) -> ApiResult<Json<Value>> {
    verify_auth(&state, &headers, None)?;
    if body.message.trim().is_empty() {
        return Err(ApiError::bad_request("commit message cannot be empty"));
    }
    let author = body.author.unwrap_or_else(|| "daemon-client".to_string());
    let branch = body.branch;
    let message = body.message;
    let vcs = state.vcs.clone();
    let branch_clone = branch.clone();
    let msg_clone = message.clone();
    let author_clone = author.clone();

    let commit_id =
        run_blocking(move || vcs.commit(&branch_clone, &author_clone, &msg_clone)).await?;
    let payload = json!({
        "branch": branch,
        "commit_id": commit_id.to_hex(),
        "author": author,
        "message": message,
    });
    state.emit("committed", &branch, payload.clone());
    Ok(Json(payload))
}

#[derive(Deserialize)]
struct LogQuery {
    branch: Option<String>,
    limit: Option<usize>,
    token: Option<String>,
}

async fn list_commits(
    State(state): State<DaemonState>,
    headers: HeaderMap,
    Query(q): Query<LogQuery>,
) -> ApiResult<Json<Value>> {
    verify_auth(&state, &headers, q.token.as_deref())?;
    let branch = q.branch.unwrap_or_else(|| "main".to_string());
    let limit = q.limit.unwrap_or(20).min(500);
    let vcs = state.vcs.clone();

    let commits = run_blocking(move || vcs.log(&branch, limit)).await?;
    Ok(Json(json!(commits)))
}

#[derive(Deserialize)]
struct TokenQuery {
    token: Option<String>,
}

async fn list_branches(
    State(state): State<DaemonState>,
    headers: HeaderMap,
    Query(q): Query<TokenQuery>,
) -> ApiResult<Json<Value>> {
    verify_auth(&state, &headers, q.token.as_deref())?;
    let vcs = state.vcs.clone();
    let branches = run_blocking(move || {
        let refs = vcs.store().list_refs()?;
        let out: Vec<Value> = refs
            .into_iter()
            .map(|(name, cid)| json!({ "branch": name, "commit_id": cid.to_hex() }))
            .collect();
        Ok(json!(out))
    })
    .await?;
    Ok(Json(branches))
}

#[derive(Deserialize)]
struct CreateBranchBody {
    name: String,
    from: Option<String>,
}

async fn create_branch(
    State(state): State<DaemonState>,
    headers: HeaderMap,
    Json(body): Json<CreateBranchBody>,
) -> ApiResult<Json<Value>> {
    verify_auth(&state, &headers, None)?;
    let from_rev = match body.from.as_deref().filter(|s| !s.trim().is_empty()) {
        Some(raw) => parse_revision(raw),
        None => Revision::BranchHead("main".to_string()),
    };
    let name = body.name;
    let name_clone = name.clone();
    let vcs = state.vcs.clone();

    run_blocking(move || vcs.create_branch(&name_clone, &from_rev)).await?;
    let payload = json!({ "created": name });
    state.emit("branch_created", &name, payload.clone());
    Ok(Json(payload))
}

#[derive(Deserialize)]
struct DeleteBranchQuery {
    name: Option<String>,
    token: Option<String>,
}

async fn delete_branch_query(
    State(state): State<DaemonState>,
    headers: HeaderMap,
    Query(q): Query<DeleteBranchQuery>,
) -> ApiResult<Json<Value>> {
    verify_auth(&state, &headers, q.token.as_deref())?;
    let name = q
        .name
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| ApiError::bad_request("query parameter 'name' is required"))?;
    perform_delete_branch(state, name).await
}

async fn delete_branch_path(
    State(state): State<DaemonState>,
    headers: HeaderMap,
    Path(name): Path<String>,
    Query(q): Query<TokenQuery>,
) -> ApiResult<Json<Value>> {
    verify_auth(&state, &headers, q.token.as_deref())?;
    perform_delete_branch(state, name).await
}

async fn perform_delete_branch(state: DaemonState, name: String) -> ApiResult<Json<Value>> {
    let vcs = state.vcs.clone();
    let name_clone = name.clone();
    run_blocking(move || vcs.delete_branch(&name_clone)).await?;
    let payload = json!({ "deleted": name });
    state.emit("branch_deleted", &name, payload.clone());
    Ok(Json(payload))
}

#[derive(Deserialize)]
struct RewindBody {
    commit_id: String,
    #[serde(default = "default_main")]
    branch: String,
}

async fn rewind_branch(
    State(state): State<DaemonState>,
    headers: HeaderMap,
    Json(body): Json<RewindBody>,
) -> ApiResult<Json<Value>> {
    verify_auth(&state, &headers, None)?;
    let cid = CommitId::from_hex(&body.commit_id)?;
    let branch = body.branch;
    let branch_clone = branch.clone();
    let vcs = state.vcs.clone();

    run_blocking(move || vcs.rewind(&branch_clone, &cid)).await?;
    let payload = json!({ "branch": branch, "rewound_to": cid.to_hex() });
    state.emit("rewound", &branch, payload.clone());
    Ok(Json(payload))
}

#[derive(Deserialize)]
struct DiffQuery {
    base: String,
    target: String,
    token: Option<String>,
}

async fn diff_revisions(
    State(state): State<DaemonState>,
    headers: HeaderMap,
    Query(q): Query<DiffQuery>,
) -> ApiResult<Json<Value>> {
    verify_auth(&state, &headers, q.token.as_deref())?;
    let base = parse_revision(&q.base);
    let target = parse_revision(&q.target);
    let vcs = state.vcs.clone();

    let diffs = run_blocking(move || vcs.diff(&base, &target)).await?;
    Ok(Json(json!(diffs)))
}

#[derive(Deserialize)]
struct MergeBody {
    source: String,
    #[serde(default = "default_main")]
    target: String,
    strategy: Option<String>,
}

async fn merge_branches(
    State(state): State<DaemonState>,
    headers: HeaderMap,
    Json(body): Json<MergeBody>,
) -> ApiResult<Json<Value>> {
    verify_auth(&state, &headers, None)?;
    let strategy = match body.strategy.as_deref() {
        Some("ours") => MergeStrategy::Ours,
        Some("theirs") => MergeStrategy::Theirs,
        _ => MergeStrategy::Auto,
    };
    let vcs = state.vcs.clone();
    let src = body.source;
    let dst = body.target;
    let dst_clone = dst.clone();
    let src_clone = src.clone();

    let outcome = run_blocking(move || vcs.merge(&src_clone, &dst_clone, strategy)).await?;
    let payload = match outcome {
        MergeOutcome::FastForward(cid) => {
            let p = json!({ "outcome": "fast_forward", "commit_id": cid.to_hex(), "source": src, "target": dst });
            state.emit("merged", &dst, p.clone());
            p
        }
        MergeOutcome::Merged(cid) => {
            let p = json!({ "outcome": "merged", "commit_id": cid.to_hex(), "source": src, "target": dst });
            state.emit("merged", &dst, p.clone());
            p
        }
        MergeOutcome::Conflict {
            base_commit,
            conflicts,
        } => json!({
            "outcome": "conflict",
            "base_commit": base_commit.to_hex(),
            "conflicts": conflicts,
        }),
    };
    Ok(Json(payload))
}

async fn run_gc(
    State(state): State<DaemonState>,
    headers: HeaderMap,
    Query(q): Query<TokenQuery>,
) -> ApiResult<Json<Value>> {
    verify_auth(&state, &headers, q.token.as_deref())?;
    let vcs = state.vcs.clone();
    let stats = run_blocking(move || vcs.gc()).await?;
    Ok(Json(json!(stats)))
}

#[derive(Deserialize)]
struct SearchBody {
    query: String,
    #[serde(default = "default_main")]
    branch: String,
    prefix: Option<String>,
    top_k: Option<usize>,
}

async fn search_memories(
    State(state): State<DaemonState>,
    headers: HeaderMap,
    Json(body): Json<SearchBody>,
) -> ApiResult<Json<Value>> {
    verify_auth(&state, &headers, None)?;
    let prefix = body
        .prefix
        .as_deref()
        .filter(|s| !s.trim().is_empty())
        .map(VirtualPath::new)
        .transpose()?;
    let mut req = SearchRequest::new(Revision::BranchWorkspace(body.branch), body.query)
        .with_top_k(body.top_k.unwrap_or(10));
    if let Some(p) = prefix {
        req = req.with_prefix(p);
    }
    let search = state.search.clone();
    let hits = run_blocking(move || search.search(&req)).await?;
    Ok(Json(json!(hits)))
}

#[derive(Deserialize)]
struct CompileBody {
    #[serde(default = "default_main")]
    branch: String,
    max_tokens: Option<usize>,
}

async fn compile_core_route(
    State(state): State<DaemonState>,
    headers: HeaderMap,
    Json(body): Json<CompileBody>,
) -> ApiResult<Json<Value>> {
    verify_auth(&state, &headers, None)?;
    let search = state.search.clone();
    let max_tokens = body.max_tokens.unwrap_or(2000);
    let rev = Revision::BranchWorkspace(body.branch);
    let compiled = run_blocking(move || search.compile_core(&rev, max_tokens)).await?;
    Ok(Json(json!(compiled)))
}

#[derive(Deserialize)]
struct IngestBody {
    text: String,
    role: Option<String>,
    #[serde(default = "default_main")]
    branch: String,
    draft: Option<bool>,
    title: Option<String>,
    path: Option<String>,
    tier: Option<String>,
    tags: Option<Vec<String>>,
    author: Option<String>,
    importance: Option<f32>,
    commit: Option<bool>,
    message: Option<String>,
    use_llm: Option<bool>,
}

async fn ingest_note(
    State(state): State<DaemonState>,
    headers: HeaderMap,
    Json(body): Json<IngestBody>,
) -> ApiResult<Json<Value>> {
    verify_auth(&state, &headers, None)?;
    let role = match body.role.as_deref() {
        Some(r) => IngestRole::parse(r)?,
        None => IngestRole::Dev,
    };
    let mut req = IngestRequest::new(role, body.text)
        .with_branch(body.branch)
        .with_draft(body.draft.unwrap_or(false))
        .with_commit(body.commit.unwrap_or(true))
        .with_use_llm(body.use_llm.unwrap_or(false));

    if let Some(t) = body.title {
        req = req.with_title(t);
    }
    if let Some(p) = body.path.as_deref().filter(|s| !s.trim().is_empty()) {
        let vp = VirtualPath::new(p)?;
        vp.validate_file_path()?;
        req = req.with_path(vp);
    }
    if let Some(raw_tier) = body.tier.as_deref().filter(|s| !s.trim().is_empty()) {
        let tier = MemoryTier::from_segment(raw_tier)
            .ok_or_else(|| ApiError::bad_request(format!("unknown tier '{raw_tier}'")))?;
        req = req.with_tier(tier);
    }
    if let Some(tags) = body.tags {
        req = req.with_tags(tags);
    }
    if let Some(author) = body.author {
        req = req.with_author(author);
    }
    if let Some(imp) = body.importance {
        req = req.with_importance(imp);
    }
    if let Some(msg) = body.message {
        req = req.with_commit_message(msg);
    }

    let search = state.search.clone();
    let llm_cfg = state.llm_cfg.clone();
    let outcome = run_blocking(move || search.ingest(&req, Some(&llm_cfg))).await?;

    state.emit(
        "ingested",
        &outcome.branch,
        json!({
            "branch": outcome.branch,
            "path": outcome.path,
            "blob_id": outcome.blob_id,
            "commit_id": outcome.commit_id,
            "title": outcome.title,
        }),
    );
    Ok(Json(json!(outcome)))
}

#[derive(Deserialize)]
struct ConsolidateBody {
    #[serde(default = "default_main")]
    branch: String,
    consolidation_branch: Option<String>,
    prefix: Option<String>,
    since_ms: Option<i64>,
    max_notes: Option<usize>,
    min_importance: Option<f32>,
    use_llm: Option<bool>,
    strict_llm: Option<bool>,
    auto_merge: Option<bool>,
    strategy: Option<String>,
    delete_branch: Option<bool>,
    archive_processed: Option<bool>,
    decay_factor: Option<f32>,
    dry_run: Option<bool>,
}

async fn consolidate_branch(
    State(state): State<DaemonState>,
    headers: HeaderMap,
    Json(body): Json<ConsolidateBody>,
) -> ApiResult<Json<Value>> {
    verify_auth(&state, &headers, None)?;
    let mut req = ConsolidationRequest::new(body.branch);
    if let Some(cb) = body.consolidation_branch {
        req = req.with_consolidation_branch(cb);
    }
    if let Some(p) = body.prefix.as_deref().filter(|s| !s.trim().is_empty()) {
        req = req.with_episodic_prefix(VirtualPath::new(p)?);
    }
    if let Some(since) = body.since_ms {
        req = req.with_since_ms(since);
    }
    if let Some(max_notes) = body.max_notes {
        req = req.with_max_notes(max_notes);
    }
    if let Some(min_imp) = body.min_importance {
        req = req.with_min_importance(min_imp);
    }
    if let Some(use_llm) = body.use_llm {
        req = req.with_use_llm(use_llm);
    }
    if let Some(strict) = body.strict_llm {
        req = req.with_strict_llm(strict);
    }
    if let Some(auto_merge) = body.auto_merge {
        req = req.with_auto_merge(auto_merge);
    }
    if let Some(strategy) = body.strategy.as_deref() {
        let parsed = match strategy {
            "ours" => MergeStrategy::Ours,
            "theirs" => MergeStrategy::Theirs,
            _ => MergeStrategy::Auto,
        };
        req = req.with_merge_strategy(parsed);
    }
    if let Some(del) = body.delete_branch {
        req = req.with_delete_branch_on_merge(del);
    }
    if let Some(arch) = body.archive_processed {
        req = req.with_archive_processed(arch);
    }
    if let Some(decay) = body.decay_factor {
        req = req.with_decay_factor(decay);
    }
    if let Some(dry) = body.dry_run {
        req = req.with_dry_run(dry);
    }

    let search = state.search.clone();
    let llm_cfg = state.llm_cfg.clone();
    let report = run_blocking(move || {
        ConsolidationEngine::new(search)
            .with_llm_config(llm_cfg)
            .consolidate(&req)
    })
    .await?;

    if !report.dry_run && report.episodes_scanned > 0 {
        let emitted_branch = match &report.merge_status {
            ConsolidationMergeStatus::FastForward { .. }
            | ConsolidationMergeStatus::Merged { .. } => report.source_branch.clone(),
            _ => report
                .consolidation_branch
                .clone()
                .unwrap_or_else(|| report.source_branch.clone()),
        };
        state.emit(
            "consolidated",
            emitted_branch,
            json!({
                "source_branch": report.source_branch,
                "consolidation_branch": report.consolidation_branch,
                "episodes_scanned": report.episodes_scanned,
                "cards_distilled": report.distilled_cards.len(),
                "merge_status": report.merge_status,
            }),
        );
    }

    Ok(Json(json!(report)))
}

#[derive(Deserialize)]
struct EventsQuery {
    branch: Option<String>,
    token: Option<String>,
}

async fn stream_events(
    State(state): State<DaemonState>,
    headers: HeaderMap,
    Query(q): Query<EventsQuery>,
) -> ApiResult<Sse<impl Stream<Item = std::result::Result<Event, Infallible>>>> {
    verify_auth(&state, &headers, q.token.as_deref())?;
    let filter_branch = q.branch.filter(|s| !s.trim().is_empty());
    let rx = state.event_tx.subscribe();

    let stream = BroadcastStream::new(rx).filter_map(move |item| match item {
        Ok(ev) => {
            if let Some(ref b) = filter_branch {
                if &ev.branch != b {
                    return None;
                }
            }
            let payload = serde_json::to_string(&ev).unwrap_or_else(|_| "{}".to_string());
            Some(Ok(Event::default().event(ev.event).data(payload)))
        }
        Err(_) => None,
    });

    Ok(Sse::new(stream).keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(15))
            .text("keep-alive"),
    ))
}

async fn mcp_http_post(
    State(state): State<DaemonState>,
    headers: HeaderMap,
    Query(q): Query<TokenQuery>,
    Json(req): Json<Value>,
) -> ApiResult<Response> {
    verify_auth(&state, &headers, q.token.as_deref())?;
    let vcs = state.vcs.clone();
    let search = state.search.clone();
    let llm_cfg = state.llm_cfg.clone();

    let resp_opt =
        tokio::task::spawn_blocking(move || handle_jsonrpc_request(&vcs, &search, &llm_cfg, &req))
            .await
            .map_err(|e| ApiError::internal(format!("mcp task panicked: {e}")))?;

    match resp_opt {
        Some(resp) => Ok((StatusCode::OK, Json(resp)).into_response()),
        None => Ok(StatusCode::ACCEPTED.into_response()),
    }
}

struct McpSessionGuard {
    session_id: String,
    sessions: Arc<RwLock<HashMap<String, mpsc::Sender<String>>>>,
}

impl Drop for McpSessionGuard {
    fn drop(&mut self) {
        let session_id = self.session_id.clone();
        let sessions = Arc::clone(&self.sessions);
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                sessions.write().await.remove(&session_id);
            });
        }
    }
}

async fn mcp_sse_connect(
    State(state): State<DaemonState>,
    headers: HeaderMap,
    Query(q): Query<TokenQuery>,
) -> ApiResult<Sse<impl Stream<Item = std::result::Result<Event, Infallible>>>> {
    verify_auth(&state, &headers, q.token.as_deref())?;
    let session_id = Uuid::new_v4().to_string();
    let (tx, rx) = mpsc::channel::<String>(64);
    state
        .mcp_sessions
        .write()
        .await
        .insert(session_id.clone(), tx);

    let guard = McpSessionGuard {
        session_id: session_id.clone(),
        sessions: Arc::clone(&state.mcp_sessions),
    };

    let endpoint_uri = match q.token {
        Some(ref tok) if !tok.is_empty() => {
            format!("/mcp/messages?session_id={session_id}&token={tok}")
        }
        _ => format!("/mcp/messages?session_id={session_id}"),
    };

    let init_stream = stream::once(async move {
        Ok::<Event, Infallible>(Event::default().event("endpoint").data(endpoint_uri))
    });
    let msg_stream = ReceiverStream::new(rx).map(move |data| {
        let _keep_alive = &guard;
        Ok::<Event, Infallible>(Event::default().event("message").data(data))
    });
    let combined = futures_util::stream::StreamExt::chain(init_stream, msg_stream);

    Ok(Sse::new(combined).keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(15))
            .text("keep-alive"),
    ))
}

#[derive(Deserialize)]
struct McpMessageQuery {
    session_id: String,
    token: Option<String>,
}

async fn mcp_sse_message(
    State(state): State<DaemonState>,
    headers: HeaderMap,
    Query(q): Query<McpMessageQuery>,
    Json(req): Json<Value>,
) -> ApiResult<Response> {
    verify_auth(&state, &headers, q.token.as_deref())?;
    let tx_opt = {
        let sessions = state.mcp_sessions.read().await;
        sessions.get(&q.session_id).cloned()
    };
    let Some(tx) = tx_opt else {
        return Err(ApiError::not_found(format!(
            "unknown MCP SSE session '{}'",
            q.session_id
        )));
    };

    let vcs = state.vcs.clone();
    let search = state.search.clone();
    let llm_cfg = state.llm_cfg.clone();
    let resp_opt =
        tokio::task::spawn_blocking(move || handle_jsonrpc_request(&vcs, &search, &llm_cfg, &req))
            .await
            .map_err(|e| ApiError::internal(format!("mcp sse task panicked: {e}")))?;

    if let Some(resp) = resp_opt {
        let serialized = serde_json::to_string(&resp)
            .map_err(|e| ApiError::internal(format!("failed to serialize mcp response: {e}")))?;
        if tx.send(serialized).await.is_err() {
            state.mcp_sessions.write().await.remove(&q.session_id);
            return Err(ApiError::not_found("MCP SSE session closed"));
        }
    }

    Ok((StatusCode::ACCEPTED, Json(json!({ "accepted": true }))).into_response())
}

#[derive(Deserialize)]
struct GraphQuery {
    branch: Option<String>,
    token: Option<String>,
}

async fn get_branch_graph(
    State(state): State<DaemonState>,
    headers: HeaderMap,
    Query(q): Query<GraphQuery>,
) -> ApiResult<Json<Value>> {
    verify_auth(&state, &headers, q.token.as_deref())?;
    let branch = q.branch.unwrap_or_else(|| "main".to_string());
    let search = state.search.clone();

    let graph =
        run_blocking(move || search.branch_graph(&Revision::BranchWorkspace(branch))).await?;
    Ok(Json(json!(graph)))
}

fn default_main() -> String {
    "main".to_string()
}

fn parse_revision(raw: &str) -> Revision {
    if let Ok(cid) = CommitId::from_hex(raw) {
        Revision::Commit(cid)
    } else {
        Revision::BranchWorkspace(raw.to_string())
    }
}
