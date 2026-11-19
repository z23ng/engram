//! REST endpoints that change a project: commit, delete, image upload, branch, merge.

use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{delete, post, put};
use axum::{Json, Router};
use mem_core::{MemoryNode, MergeOutcome, MergeStrategy, Revision, VirtualPath};
use mem_vcs::VersionControlEngine;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::davfs::{attachment_type, MAX_FILE};
use crate::http::{guarded, Shared};
use crate::{Action, ServerError};

pub(crate) fn routes() -> Router<Shared> {
    let upload_route = put(upload).layer(DefaultBodyLimit::max(MAX_FILE));
    Router::new()
        .route("/api/v1/projects/{project}/commit", post(commit))
        .route("/api/v1/projects/{project}/files", delete(delete_file))
        .route("/api/v1/projects/{project}/raw", upload_route)
        .route("/api/v1/projects/{project}/branches", post(create_branch))
        .route("/api/v1/projects/{project}/merge", post(merge))
}

type JsonResult = Result<Json<Value>, ServerError>;
type StatusJson = Result<(StatusCode, Json<Value>), ServerError>;

fn main_or(branch: Option<String>) -> String {
    branch.unwrap_or_else(|| "main".into())
}

#[derive(Deserialize)]
struct CommitBody {
    branch: Option<String>,
    message: String,
}

async fn commit(
    State(cp): State<Shared>,
    Path(project): Path<String>,
    headers: HeaderMap,
    Json(body): Json<CommitBody>,
) -> JsonResult {
    let scope = project.clone();
    guarded(cp, headers, scope, Action::Write, move |cp, key, engine| {
        let branch = main_or(body.branch);
        if engine.vcs().store().workspace_list(&branch)?.is_empty() {
            return Err(ServerError::BadRequest(format!(
                "nothing to commit on '{branch}'"
            )));
        }
        let id = engine
            .vcs()
            .commit(&branch, &key.name, &body.message)?
            .to_hex();
        cp.record_audit(key, &project, "rest.commit", Some(&id))?;
        Ok(Json(json!({"commit_id": id, "branch": branch})))
    })
    .await
}

#[derive(Deserialize)]
struct FileQuery {
    path: String,
    branch: Option<String>,
}

async fn delete_file(
    State(cp): State<Shared>,
    Path(project): Path<String>,
    headers: HeaderMap,
    Query(q): Query<FileQuery>,
) -> JsonResult {
    let scope = project.clone();
    guarded(cp, headers, scope, Action::Write, move |cp, key, engine| {
        let path = VirtualPath::new(&q.path)?;
        let deleted = engine.vcs().delete_path(&main_or(q.branch), &path)?;
        if deleted == 0 {
            return Err(ServerError::NotFound(format!(
                "'{}' not found",
                path.as_str()
            )));
        }
        cp.record_audit(key, &project, "rest.delete", None)?;
        Ok(Json(json!({"deleted": deleted})))
    })
    .await
}

/// Stores an image below `<tier>/_attachments/` (same rule as WebDAV); it is not indexed.
async fn upload(
    State(cp): State<Shared>,
    Path(project): Path<String>,
    headers: HeaderMap,
    Query(q): Query<FileQuery>,
    body: Bytes,
) -> JsonResult {
    let scope = project.clone();
    guarded(cp, headers, scope, Action::Write, move |cp, key, engine| {
        let path = VirtualPath::new(&q.path)?;
        let content_type = attachment_type(&path).ok_or_else(|| {
            ServerError::Forbidden(
                "only png/jpg/gif/webp images below <tier>/_attachments/ (not core) can be uploaded"
                    .into(),
            )
        })?;
        let mut node = MemoryNode::new(path.clone(), body.to_vec());
        node.metadata.content_type = content_type.into();
        let blob_id = engine.vcs().write_node(&main_or(q.branch), node)?;
        cp.record_audit(key, &project, "rest.upload", None)?;
        Ok(Json(json!({"path": path, "blob_id": blob_id})))
    })
    .await
}

#[derive(Deserialize)]
struct NewBranch {
    name: String,
    from: Option<String>,
}

async fn create_branch(
    State(cp): State<Shared>,
    Path(project): Path<String>,
    headers: HeaderMap,
    Json(body): Json<NewBranch>,
) -> StatusJson {
    let scope = project.clone();
    guarded(
        cp,
        headers,
        scope,
        Action::Branch,
        move |cp, key, engine| {
            if body.name.contains('~') {
                // WebDAV URLs use '~' for '/', so a literal '~' would make the branch unreachable.
                return Err(ServerError::BadRequest(
                    "branch names must not contain '~'".into(),
                ));
            }
            let from = Revision::BranchHead(main_or(body.from));
            engine.vcs().create_branch(&body.name, &from)?;
            cp.record_audit(key, &project, "rest.branch", None)?;
            Ok((StatusCode::CREATED, Json(json!({"name": body.name}))))
        },
    )
    .await
}

#[derive(Deserialize)]
struct MergeBody {
    source: String,
    target: Option<String>,
    strategy: Option<MergeStrategy>,
}

/// Merges `source` into `target` (default `main`); a conflict returns 409 and changes nothing.
async fn merge(
    State(cp): State<Shared>,
    Path(project): Path<String>,
    headers: HeaderMap,
    Json(body): Json<MergeBody>,
) -> StatusJson {
    let scope = project.clone();
    guarded(
        cp,
        headers,
        scope,
        Action::Branch,
        move |cp, key, engine| {
            let target = main_or(body.target);
            let strategy = body.strategy.unwrap_or(MergeStrategy::Auto);
            let (outcome, id) =
                match engine
                    .vcs()
                    .merge_as(&body.source, &target, strategy, &key.name)?
                {
                    MergeOutcome::FastForward(id) => ("fast_forward", id),
                    MergeOutcome::Merged(id) => ("merged", id),
                    MergeOutcome::Conflict {
                        base_commit,
                        conflicts,
                    } => {
                        let body = json!({"outcome": "conflict", "base_commit": base_commit,
                    "conflicts": conflicts});
                        return Ok((StatusCode::CONFLICT, Json(body)));
                    }
                };
            cp.record_audit(key, &project, "rest.merge", Some(&id.to_hex()))?;
            let body = json!({"outcome": outcome, "commit_id": id, "target": target});
            Ok((StatusCode::OK, Json(body)))
        },
    )
    .await
}
