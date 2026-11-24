//! REST endpoints for the web client: identity, projects, keys, and audit.

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{delete, get};
use axum::{Json, Router};
use chrono::Utc;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::http::{guarded, keyed, Shared};
use crate::{Action, ApiKey, Role, ServerError};

pub(crate) fn routes() -> Router<Shared> {
    Router::new()
        .route("/api/v1/me", get(me))
        .route("/api/v1/projects", get(projects).post(create_project))
        .route("/api/v1/keys", get(keys).post(create_key))
        .route("/api/v1/keys/{id}", delete(revoke_key))
        .route("/api/v1/projects/{project}/audit", get(audit))
}

type JsonResult = Result<Json<Value>, ServerError>;
type Created = Result<(StatusCode, Json<Value>), ServerError>;

/// Key creation and project creation need an admin key that is not limited to some projects.
fn global_admin(key: &ApiKey) -> Result<(), ServerError> {
    if key.role == Role::Admin && key.projects.is_none() {
        Ok(())
    } else {
        Err(ServerError::Forbidden(
            "a global admin key is required".into(),
        ))
    }
}

fn key_json(k: &ApiKey) -> Value {
    json!({"id": k.id, "name": k.name, "role": k.role.as_str(), "projects": k.projects,
        "expires_at_ms": k.expires_at_ms, "revoked": k.revoked})
}

async fn me(State(cp): State<Shared>, headers: HeaderMap) -> JsonResult {
    keyed(cp, headers, |_, k| {
        let me = json!({"name": k.name, "role": k.role.as_str(), "projects": k.projects});
        Ok(Json(me))
    })
    .await
}

async fn projects(State(cp): State<Shared>, headers: HeaderMap) -> JsonResult {
    keyed(cp, headers, |cp, k| {
        let visible: Vec<String> = cp
            .list_projects()?
            .into_iter()
            .filter(|p| k.projects.as_ref().is_none_or(|scope| scope.contains(p)))
            .collect();
        Ok(Json(json!({"projects": visible})))
    })
    .await
}

#[derive(Deserialize)]
struct NewProject {
    slug: String,
}

async fn create_project(
    State(cp): State<Shared>,
    headers: HeaderMap,
    Json(body): Json<NewProject>,
) -> Created {
    keyed(cp, headers, move |cp, k| {
        global_admin(k)?;
        cp.create_project(&body.slug)?;
        cp.record_audit(k, &body.slug, "project.create", None)?;
        Ok((StatusCode::CREATED, Json(json!({"slug": body.slug}))))
    })
    .await
}

async fn keys(State(cp): State<Shared>, headers: HeaderMap) -> JsonResult {
    keyed(cp, headers, |cp, k| {
        global_admin(k)?;
        let keys: Vec<Value> = cp.list_keys()?.iter().map(key_json).collect();
        Ok(Json(json!({"keys": keys})))
    })
    .await
}

#[derive(Deserialize)]
struct NewKey {
    name: String,
    role: String,
    projects: Option<Vec<String>>,
    expires_days: Option<i64>,
}

async fn create_key(
    State(cp): State<Shared>,
    headers: HeaderMap,
    Json(body): Json<NewKey>,
) -> Created {
    keyed(cp, headers, move |cp, k| {
        global_admin(k)?;
        let bad = |m: String| Err(ServerError::BadRequest(m));
        let Some(role) = Role::parse(&body.role) else {
            return bad(format!("unknown role '{}'", body.role));
        };
        let name = body.name.trim();
        if name.is_empty() {
            return bad("key name must not be empty".into());
        }
        let existing = cp.list_projects()?;
        if let Some(p) = body
            .projects
            .iter()
            .flatten()
            .find(|p| !existing.contains(p))
        {
            return bad(format!("project '{p}' does not exist"));
        }
        let expires = body
            .expires_days
            .map(|d| Utc::now().timestamp_millis() + d * 86_400_000);
        let (key, secret) = cp.create_key(name, role, body.projects, expires)?;
        let created = json!({"key": key_json(&key), "secret": secret});
        Ok((StatusCode::CREATED, Json(created)))
    })
    .await
}

async fn revoke_key(
    State(cp): State<Shared>,
    headers: HeaderMap,
    Path(id): Path<i64>,
) -> Result<StatusCode, ServerError> {
    keyed(cp, headers, move |cp, k| {
        global_admin(k)?;
        if !cp.revoke_key(id)? {
            return Err(ServerError::NotFound(format!("active key {id} not found")));
        }
        Ok(StatusCode::NO_CONTENT)
    })
    .await
}

#[derive(Deserialize)]
struct Limit {
    limit: Option<usize>,
}

async fn audit(
    State(cp): State<Shared>,
    Path(project): Path<String>,
    headers: HeaderMap,
    Query(q): Query<Limit>,
) -> JsonResult {
    let scope = project.clone();
    guarded(cp, headers, scope, Action::Manage, move |cp, _, _| {
        let limit = q.limit.unwrap_or(100).min(1000);
        let entries: Vec<Value> = cp
            .list_audit(&project, limit)?
            .into_iter()
            .map(|a| {
                json!({"at_ms": a.at_ms, "key_id": a.key_id, "key_name": a.key_name,
                    "role": a.role, "project": a.project, "operation": a.operation,
                    "commit_id": a.commit_id})
            })
            .collect();
        Ok(Json(json!({"entries": entries})))
    })
    .await
}
